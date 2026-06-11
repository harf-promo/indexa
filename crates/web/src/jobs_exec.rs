//! Background job execution bodies — the long `run_*` scan/deep/summarize logic
//! plus their finalization and watchdog helpers. Spawned by the `api_job_*`
//! handlers; no axum routing lives here.

use crate::jobs::{broadcast_only, push, JobEvent, JobHandle, JobStatus, Jobs, PressureInfo};
use crate::AppState;
use indexa_core::{
    resource::{
        assess, pause_step, MachineSpec, PauseAction, Pressure, WatchdogState, MAX_PAUSE_SECS,
    },
    store::{ChunkRecord, EdgeRecord},
    walker::{walk, EntryKind, WalkConfig},
};
use indexa_embed::Embedder;
use indexa_llm::{Describer, Generator, OllamaLlm};
use indexa_query::{process_queue_item_with_passes, requeue_subtree, QueueOutcome, MAX_DIR_DEFERS};
use std::collections::HashMap;
use std::sync::Arc;

// ── Job runner ────────────────────────────────────────────────────────────────

/// Schedule removal of a job from the registry after 60 s. Allows refreshed
/// clients to re-subscribe to recently-finished jobs and replay history.
pub(crate) fn schedule_cleanup(jobs: Jobs, id: uuid::Uuid) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        jobs.write().await.remove(&id);
    });
}

fn finalize_failed(handle: &Arc<JobHandle>, stage: &str, err: &anyhow::Error) {
    let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
    let error = format!("{err:#}");
    push(
        handle,
        JobEvent::Failed {
            error,
            stage: Some(stage.to_owned()),
            item_path: None,
            chain: if chain.len() > 1 { Some(chain) } else { None },
            code: None,
        },
    );
    handle.set_status(JobStatus::Failed);
}

fn finalize_done(handle: &Arc<JobHandle>, summary: &str) {
    push(
        handle,
        JobEvent::Done {
            summary: summary.to_owned(),
        },
    );
    handle.set_status(JobStatus::Done);
}

/// Emit a terminal Done event noting the job was cancelled mid-run.
fn finalize_cancelled(handle: &Arc<JobHandle>, done: usize) {
    push(
        handle,
        JobEvent::Done {
            summary: format!("Cancelled after {done} items"),
        },
    );
    handle.set_status(JobStatus::Done);
}

/// Compute the swap-used percentage of total swap (0–100), saturating to 100 on no swap.
fn swap_pct(sample: &indexa_core::resource::MemSample) -> u64 {
    sample
        .swap_used_bytes
        .checked_mul(100)
        .and_then(|v| v.checked_div(sample.swap_total_bytes))
        .unwrap_or(100)
}

/// Check memory pressure before an Ollama call.
///
/// If pressure is Throttle or Critical:
///   1. Emits a calm, actionable Warning event so the user can see it in the Jobs UI.
///   2. On a **Critical** entry, unloads the resident model(s) once so their wired RAM
///      frees — that is what lets the recovery check below trigger (macOS swap is sticky
///      and never drains on its own, so we cannot wait for swap to fall).
///   3. Loops on the recover-aware [`pause_step`] predicate: resumes the moment free RAM
///      climbs back above headroom (`compute_budget > 0`) — even while swap stays high —
///      or the entry signal clears; otherwise sleeps on the 5 s/2 s cadence up to
///      [`MAX_PAUSE_SECS`], then proceeds.
///
/// `embedder` / `llm` are the handles whose models to unload on a Critical pause; the deep
/// loop always passes the embedder (and the contextual-retrieval LLM when present), and the
/// summarize loop passes its describer + embedder.
///
/// The caller should invoke this before every embedding or LLM call
/// in the hot loops of `run_deep_phase` and `run_summarize_phase`.
async fn run_watchdog_check(
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    handle: &Arc<JobHandle>,
    stage: &str,
    embedder: Option<&dyn Embedder>,
    // Unload target on a Critical pause. Typed `Describer` (not `Generator`): both the
    // summarize describer and the deep-phase context LLM implement Describer, and only
    // `unload()` — shared by both traits — is used here.
    llm: Option<&(dyn Describer + Send + Sync)>,
) {
    let sample = wdog.sample();
    // Gate entry on the SAME recover-aware predicate as resume, not raw `assess()`. macOS swap
    // is sticky: after the first event `assess()` reports Critical for the rest of the job even
    // once RAM has recovered. Using `assess()` here would re-enter the pause (warn + unload +
    // reload the model) on *every* subsequent file. `pause_step(.., 0) == Resume` means "RAM is
    // fine OR no real signal" → skip. Only when RAM is genuinely low (compute_budget <= 0) do we
    // fall through and pause.
    if pause_step(spec, &sample, headroom, 0) == PauseAction::Resume {
        return;
    }
    // RAM is genuinely low. Use `assess()` only to choose the unload gate (Critical vs Throttle).
    let pressure = assess(&sample, spec, headroom);

    let pct = swap_pct(&sample);
    push(
        handle,
        JobEvent::Warning {
            stage: stage.to_owned(),
            item_path: None,
            message: format!(
                "Low on memory (swap {pct}%). Easing off and freeing the model to keep your \
                 machine responsive — this resumes automatically. \
                 Tip: lower the workload in Settings → Resource Profile."
            ),
            // Structured snapshot so the UI can line the warning up with the live RAM gauge
            // instead of parsing the prose. Every value is already in hand here.
            pressure: Some(PressureInfo {
                level: match pressure {
                    Pressure::Critical => "critical",
                    _ => "throttle",
                }
                .to_owned(),
                swap_percent: pct,
                used_bytes: sample.used_bytes,
                budget_bytes: indexa_core::resource::compute_budget(spec, &sample, headroom),
                headroom_bytes: headroom,
            }),
        },
    );

    // On a Critical entry, unload the resident model(s) once so their wired pages free and
    // `compute_budget` can climb back above 0. macOS swap is sticky and never drains on its
    // own, so gating resume on swap level alone would stall here for the full backstop.
    if pressure == Pressure::Critical {
        if let Some(e) = embedder {
            e.unload().await;
        }
        if let Some(l) = llm {
            l.unload().await;
        }
    }

    // Wait until memory actually recovers, capped at resource::MAX_PAUSE_SECS. `pause_step`
    // re-evaluates a fresh sample each tick: it resumes when free RAM returns above headroom
    // (recovery) regardless of sticky swap, and escalation (Throttle → Critical) tightens the
    // cadence immediately.
    let mut elapsed = 0u64;
    let mut next_status_at = 30u64;
    loop {
        let s = wdog.sample();
        match pause_step(spec, &s, headroom, elapsed) {
            PauseAction::Resume => break,
            PauseAction::Proceed => {
                push(
                    handle,
                    JobEvent::Warning {
                        stage: stage.to_owned(),
                        item_path: None,
                        message: format!(
                            "Memory didn't recover within {MAX_PAUSE_SECS}s — continuing gently. \
                             If this repeats, lower the workload in Settings → Resource Profile."
                        ),
                        pressure: None,
                    },
                );
                break;
            }
            PauseAction::Sleep(secs) => {
                // Emit a calm follow-up roughly every 30 s so the user isn't left wondering.
                if elapsed >= next_status_at {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: stage.to_owned(),
                            item_path: None,
                            message: format!(
                                "Still easing off while memory recovers … ({elapsed}s)"
                            ),
                            pressure: None,
                        },
                    );
                    next_status_at += 30;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
                elapsed += secs;
            }
        }
    }
}

/// Walk a path in a blocking thread; on failure, push the error to the job and return None.
/// Acquires a permit from `sem` to limit concurrent walks and prevent rayon pool starvation.
async fn walk_for_job(
    path: &str,
    handle: &Arc<JobHandle>,
    sem: &tokio::sync::Semaphore,
) -> Option<Vec<indexa_core::walker::Entry>> {
    let _permit = sem.acquire().await.ok()?;
    let pb = std::path::PathBuf::from(path);
    let walked = tokio::task::spawn_blocking(move || walk(&pb, &WalkConfig::default()))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!(e)));
    match walked {
        Ok(e) => Some(e),
        Err(e) => {
            finalize_failed(handle, "walk", &e);
            None
        }
    }
}

pub(crate) async fn run_index_job(
    state: AppState,
    path: String,
    handle: Arc<JobHandle>,
    model_override: Option<(String, String, u32)>,
) {
    // Phase 1: scan
    let Some(entries) = walk_for_job(&path, &handle, &state.walk_semaphore).await else {
        return;
    };

    if !run_scan_phase_with_entries(&state, &path, &entries, &handle).await {
        return;
    }
    // Cancellation requested during/after scan — stop before the expensive phases.
    if handle.is_cancelled() {
        finalize_cancelled(&handle, 0);
        return;
    }

    // Phase 2: deep index (its own loop also honors cancellation and emits the
    // terminal event, in which case it returns false and we just stop here).
    if !run_deep_phase(&state, &path, &entries, &handle).await {
        return;
    }
    if handle.is_cancelled() {
        finalize_cancelled(&handle, 0);
        return;
    }

    // Phase 3: summarize
    run_summarize_phase(&state, &path, None, &handle, model_override).await;
}

/// Standalone scan: walks, scans, then finalises the job as done.
pub(crate) async fn run_scan_phase_standalone(
    state: &AppState,
    path: &str,
    handle: &Arc<JobHandle>,
) {
    let Some(entries) = walk_for_job(path, handle, &state.walk_semaphore).await else {
        return;
    };
    if run_scan_phase_with_entries(state, path, &entries, handle).await {
        let n = entries.len() as u64;
        finalize_done(handle, &format!("{n} entries scanned"));
    }
}

/// Standalone deep: walks, deep-indexes, then finalises the job as done.
pub(crate) async fn run_deep_phase_standalone(
    state: &AppState,
    path: &str,
    handle: &Arc<JobHandle>,
) {
    let Some(entries) = walk_for_job(path, handle, &state.walk_semaphore).await else {
        return;
    };
    let n_files = entries.iter().filter(|e| e.kind == EntryKind::File).count();
    if run_deep_phase(state, path, &entries, handle).await {
        finalize_done(handle, &format!("Deep index complete: {n_files} files"));
    }
}

async fn run_scan_phase_with_entries(
    state: &AppState,
    path: &str,
    entries: &[indexa_core::walker::Entry],
    handle: &Arc<JobHandle>,
) -> bool {
    let n = entries.len() as u64;
    push(
        handle,
        JobEvent::Start {
            kind: "scan".into(),
            path: path.to_owned(),
            total: Some(n),
        },
    );

    let live_paths: std::collections::HashSet<String> = entries
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();

    let mut store = state.store.lock().await;
    if let Err(e) = store.upsert_entries(entries) {
        finalize_failed(handle, "scan", &e);
        return false;
    }
    if let Err(e) = store.reconcile_entries(path, &live_paths) {
        push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("{e:#}"),
                pressure: None,
            },
        );
    }
    drop(store);

    push(
        handle,
        JobEvent::Progress {
            current: n,
            total: n,
            note: Some(format!("{n} entries scanned")),
            current_path: None,
            items_per_sec: None,
            eta_secs: None,
        },
    );
    true
}

/// Returns true on success.
async fn run_deep_phase(
    state: &AppState,
    path: &str,
    entries: &[indexa_core::walker::Entry],
    handle: &Arc<JobHandle>,
) -> bool {
    let files: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .collect();
    let n_files = files.len() as u64;
    let total_bytes: u64 = files.iter().map(|e| e.size).sum();

    push(
        handle,
        JobEvent::Start {
            kind: "deep".into(),
            path: path.to_owned(),
            total: Some(n_files),
        },
    );
    push(
        handle,
        JobEvent::Snapshot {
            count: n_files,
            bytes: total_bytes,
        },
    );

    let embed_model = state.config.embedding.model.clone();
    let cfg = state.config.describer.clone();
    let resource_cfg = state.config.resource.clone();
    let spec = state.machine_spec.clone();
    let headroom = resource_cfg.effective_headroom_bytes();

    // Build a contextual-retrieval LLM if the feature is enabled.
    let ctx_llm: Option<OllamaLlm> = if cfg.contextual_retrieval {
        let base_url = OllamaLlm::resolve_base_url(Some(&cfg.base_url));
        Some(OllamaLlm::new(&base_url, &cfg.file_model).with_num_ctx(cfg.num_ctx))
    } else {
        None
    };

    // Optional video frame captioning (opt-in, v0.16).
    let video_caption = state.config.parsers.video.caption;
    // Optional image captioning (opt-in): a vision model adds a caption chunk per image.
    // The same OllamaLlm handle drives BOTH image and video captioning, so build it when
    // EITHER is enabled — otherwise enabling only `video.caption` would silently no-op
    // (frames extracted, nothing captioned). The image caption model is used as the handle's
    // default; per-frame video calls pass `video_model` explicitly.
    let image_caption = state.config.parsers.image.caption;
    let captioner: Option<OllamaLlm> = if image_caption || video_caption {
        let base_url = OllamaLlm::resolve_base_url(Some(&cfg.base_url));
        Some(
            OllamaLlm::new(&base_url, state.config.parsers.image.caption_model())
                .with_num_ctx(cfg.num_ctx),
        )
    } else {
        None
    };
    let caption_model = state.config.parsers.image.caption_model().to_owned();
    // Optional audio transcription (opt-in): a whisper.cpp-style CLI per audio file.
    let transcribe = state.config.parsers.audio.transcribe;
    let transcribe_binary = state.config.parsers.audio.transcribe_binary().to_owned();
    let transcribe_model = state.config.parsers.audio.model.clone();
    let video_ffmpeg = state.config.parsers.video.ffmpeg_binary().to_owned();
    let video_model = state.config.parsers.video.caption_model().to_owned();
    let video_fps = state.config.parsers.video.fps();
    let video_max_frames = state.config.parsers.video.max_frames();

    // Memory watchdog: checked before each Ollama call.
    let mut wdog = WatchdogState::new();

    let mut done = 0u64;
    // M5 success tracking: distinguish "nothing to do" from "everything failed".
    let mut skipped = 0u64; // files already current (legitimate no-op)
    let mut chunks_written = 0u64; // chunks actually upserted
    let mut hard_errors = 0u64; // parse/panic/upsert failures
                                // Rolling throughput: ring buffer of (instant, items_done) samples, last ~5s.
    let mut samples: std::collections::VecDeque<(std::time::Instant, u64)> =
        std::collections::VecDeque::with_capacity(16);
    samples.push_back((std::time::Instant::now(), 0));
    let max_parse_bytes = state.config.parsers.max_file_mb.saturating_mul(1024 * 1024);

    for entry in &files {
        // Honor cancellation requested via DELETE /api/jobs/:id.
        if handle.is_cancelled() {
            finalize_cancelled(handle, done as usize);
            return false;
        }

        let path_str = entry.path.to_string_lossy().into_owned();

        // Compare against the fresh on-disk mtime from this walk, not the DB's
        // possibly-stale `modified_s`: the standalone Deep job (run_deep_phase_standalone)
        // skips the scan stage, so an edited file would otherwise be wrongly skipped.
        // Mirrors `cmd_deep`; falls back to the stored check when no mtime is available.
        let mtime_secs = entry
            .modified
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let is_current = {
            let store = state.store.lock().await;
            match mtime_secs {
                Some(m) => store
                    .chunks_current_for_mtime(&path_str, m)
                    .unwrap_or(false),
                None => store.chunks_are_current(&path_str).unwrap_or(false),
            }
        };
        if is_current {
            skipped += 1;
            done += 1;
        } else {
            let ep = entry.path.clone();
            let sz = entry.size;
            let mut extracted = match tokio::task::spawn_blocking(move || {
                indexa_parsers::registry::parse_guarded(&ep, sz, max_parse_bytes)
            })
            .await
            {
                Ok(Ok(e)) => e,
                Ok(Err(e)) => {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("{e:#}"),
                            pressure: None,
                        },
                    );
                    hard_errors += 1;
                    done += 1;
                    continue;
                }
                Err(e) => {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("parse task panicked: {e}"),
                            pressure: None,
                        },
                    );
                    hard_errors += 1;
                    done += 1;
                    continue;
                }
            };

            // Image captioning (opt-in): append a vision-model caption chunk alongside the
            // EXIF chunk. Watchdog-gated (the vision model is heavy); failure only warns.
            // Gate on `image_caption` specifically: the shared `captioner` handle is also
            // built when only video captioning is enabled, so without this guard images
            // would be captioned without the user opting in.
            if image_caption {
                if let Some(cap) = &captioner {
                    if extracted.mime.starts_with("image/") {
                        run_watchdog_check(
                            &mut wdog,
                            &spec,
                            headroom,
                            handle,
                            "deep",
                            Some(state.embedder.as_ref()),
                            Some(cap as &(dyn Describer + Send + Sync)),
                        )
                        .await;
                        match indexa_llm::caption_image_file(cap, &caption_model, &entry.path).await
                        {
                            Ok(text) if !text.trim().is_empty() => {
                                let seq = extracted.chunks.len();
                                extracted.chunks.push(indexa_parsers::types::Chunk {
                                    source: entry.path.clone(),
                                    seq,
                                    heading: "caption".to_owned(),
                                    text,
                                    language: None,
                                });
                            }
                            Ok(_) => {}
                            Err(e) => push(
                                handle,
                                JobEvent::Warning {
                                    stage: "deep".to_owned(),
                                    item_path: Some(path_str.clone()),
                                    message: format!("caption failed: {e:#}"),
                                    pressure: None,
                                },
                            ),
                        }
                    }
                }
            }

            // Audio transcription (opt-in): append a whisper transcript chunk alongside the
            // ffprobe metadata chunk. Blocking subprocess (can take minutes) → spawn_blocking
            // so it never stalls the server's async runtime.
            if transcribe && extracted.mime.starts_with("audio/") {
                let bin = transcribe_binary.clone();
                let model = transcribe_model.clone();
                let p = entry.path.clone();
                let res = tokio::task::spawn_blocking(move || {
                    indexa_parsers::media::transcribe_audio(&p, &bin, model.as_deref())
                })
                .await;
                match res {
                    Ok(Ok(text)) if !text.trim().is_empty() => {
                        let seq = extracted.chunks.len();
                        extracted.chunks.push(indexa_parsers::types::Chunk {
                            source: entry.path.clone(),
                            seq,
                            heading: "transcript".to_owned(),
                            text,
                            language: None,
                        });
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("transcription failed: {e:#}"),
                            pressure: None,
                        },
                    ),
                    Err(e) => push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("transcription task panicked: {e}"),
                            pressure: None,
                        },
                    ),
                }
            }

            // Video frame captioning (opt-in): extract frames via ffmpeg then caption
            // each frame with a local vision model, appending the combined caption as a
            // chunk. Blocking ffmpeg subprocess + async vision calls → spawn_blocking.
            if video_caption && extracted.mime.starts_with("video/") {
                let ff = video_ffmpeg.clone();
                let fps = video_fps;
                let max_fr = video_max_frames;
                let p = entry.path.clone();
                let frames_result = tokio::task::spawn_blocking(move || {
                    indexa_parsers::media::extract_video_frames(&p, &ff, fps, max_fr)
                })
                .await;
                match frames_result {
                    Ok(Ok((_dir, frame_paths))) if !frame_paths.is_empty() => {
                        let mut captions: Vec<String> = Vec::new();
                        for (i, fp) in frame_paths.iter().enumerate() {
                            match &captioner {
                                Some(llm) => {
                                    match indexa_llm::caption_image_file(llm, &video_model, fp)
                                        .await
                                    {
                                        Ok(c) if !c.trim().is_empty() => {
                                            captions.push(format!("Frame {}: {c}", i + 1));
                                        }
                                        Ok(_) => {}
                                        Err(e) => {
                                            tracing::warn!("video frame caption failed: {e:#}");
                                        }
                                    }
                                }
                                None => {
                                    // Should not happen now that the captioner is built when
                                    // video_caption is on — but warn loudly rather than silently
                                    // dropping every frame if it ever does.
                                    push(
                                        handle,
                                        JobEvent::Warning {
                                            stage: "deep".to_owned(),
                                            item_path: Some(path_str.clone()),
                                            message: "video captioning is enabled but no vision \
                                                      model is available — set parsers.video.model \
                                                      and ensure Ollama is running"
                                                .to_owned(),
                                            pressure: None,
                                        },
                                    );
                                    break;
                                }
                            }
                        }
                        if !captions.is_empty() {
                            let seq = extracted.chunks.len();
                            extracted.chunks.push(indexa_parsers::types::Chunk {
                                source: entry.path.clone(),
                                seq,
                                heading: "video captions".to_owned(),
                                text: captions.join("\n"),
                                language: None,
                            });
                        }
                    }
                    Ok(Ok(_)) => {} // no frames extracted
                    Ok(Err(e)) => push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("video frame extraction failed: {e:#}"),
                            pressure: None,
                        },
                    ),
                    Err(e) => push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("video frame task panicked: {e}"),
                            pressure: None,
                        },
                    ),
                }
            }

            if !extracted.chunks.is_empty() {
                // Build a document-level context string for contextual retrieval.
                let doc_context: Option<String> = ctx_llm.as_ref().map(|_| {
                    let joined: String = extracted
                        .chunks
                        .iter()
                        .map(|c| c.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    joined.chars().take(4000).collect()
                });

                // Phase 1 — materialize each chunk's embed text. With contextual retrieval
                // enabled this makes one (sequential) LLM blurb call per chunk; otherwise the
                // embed text is just the chunk text. The watchdog runs every iteration so both
                // the heavy blurb LLM and the batched embeds that follow are gated on memory
                // pressure (the last check is the gate before phase 2).
                let mut embed_texts: Vec<String> = Vec::with_capacity(extracted.chunks.len());
                for chunk in &extracted.chunks {
                    // Optionally prepend a context blurb generated by the file LLM.
                    let embed_text =
                        if let (Some(ref llm), Some(ref doc)) = (&ctx_llm, &doc_context) {
                            let prompt = format!(
                                "<document>\n{doc}\n</document>\n\n\
                             Here is the chunk we want to situate within the whole document:\n\
                             <chunk>\n{}\n</chunk>\n\n\
                             Give a short succinct context (1-2 sentences) to situate this chunk \
                             within the overall document for improved search retrieval. \
                             Answer only with the succinct context and nothing else.",
                                chunk.text
                            );
                            let ps = path_str.clone();
                            let model_name = cfg.file_model.clone();
                            let h = handle.clone();
                            let mut on_frag = move |frag: String| {
                                broadcast_only(
                                    &h,
                                    JobEvent::LlmFragment {
                                        item_path: ps.clone(),
                                        model: model_name.clone(),
                                        stage: "context_blurb".to_owned(),
                                        fragment: frag,
                                    },
                                );
                            };
                            match llm.generate_stream(&prompt, &mut on_frag).await {
                                Ok(blurb) => format!("{}\n\n{}", blurb.trim(), chunk.text),
                                Err(e) => {
                                    push(
                                        handle,
                                        JobEvent::Warning {
                                            stage: "deep".to_owned(),
                                            item_path: Some(path_str.clone()),
                                            message: format!("context blurb failed: {e:#}"),
                                            pressure: None,
                                        },
                                    );
                                    chunk.text.clone()
                                }
                            }
                        } else {
                            chunk.text.clone()
                        };

                    // Watchdog: pause if memory is tight before the (batched) embeds below. On a
                    // Critical pause we unload the embedder (and the contextual-retrieval LLM, if
                    // enabled) so their RAM frees and the recovery check can resume us.
                    run_watchdog_check(
                        &mut wdog,
                        &spec,
                        headroom,
                        handle,
                        "deep",
                        Some(state.embedder.as_ref()),
                        ctx_llm
                            .as_ref()
                            .map(|l| l as &(dyn Describer + Send + Sync)),
                    )
                    .await;

                    embed_texts.push(embed_text);
                }

                // Phase 2 — embed every chunk in batched round-trips (one HTTP call per
                // EMBED_BATCH_SIZE chunks instead of one per chunk), preserving order. On a batch
                // failure `embed_all` falls back per-text, recording `None` for any chunk that
                // still can't be embedded; we surface a single aggregate warning rather than one
                // event per failed chunk.
                let text_refs: Vec<&str> = embed_texts.iter().map(|s| s.as_str()).collect();
                let mut embeddings = indexa_embed::embed_all(
                    state.embedder.as_ref(),
                    &text_refs,
                    indexa_embed::EMBED_BATCH_SIZE,
                )
                .await;
                // Drop any embedding whose dim ≠ the configured `[embedding] dim` (a model/config
                // mismatch) — storing it would corrupt dense search. The chunk stays BM25-searchable.
                let (dim_mismatch, sample_dim) = indexa_embed::enforce_embedding_dim(
                    &mut embeddings,
                    state.config.embedding.dim,
                );
                if dim_mismatch > 0 {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!(
                                "{dim_mismatch} chunk(s) embedded at dim {} ≠ configured {} — stored \
                                 text-only; fix [embedding] model/dim and re-run deep",
                                sample_dim.unwrap_or(0),
                                state.config.embedding.dim
                            ),
                            pressure: None,
                        },
                    );
                }
                let embed_failures = embeddings.iter().filter(|e| e.is_none()).count();
                if embed_failures > 0 {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!(
                                "{embed_failures}/{} chunks failed to embed",
                                embeddings.len()
                            ),
                            pressure: None,
                        },
                    );
                }

                let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
                for (chunk, embedding) in extracted.chunks.iter().zip(embeddings) {
                    chunk_records.push(ChunkRecord {
                        entry_path: path_str.clone(),
                        seq: chunk.seq,
                        heading: chunk.heading.clone(),
                        text: chunk.text.clone(), // store original text, embed enriched
                        language: chunk.language.clone(),
                        embedding,
                        embed_model: Some(embed_model.clone()),
                    });
                }
                let mut store = state.store.lock().await;
                match store.upsert_chunks(&chunk_records) {
                    Ok(()) => chunks_written += chunk_records.len() as u64,
                    Err(e) => {
                        push(
                            handle,
                            JobEvent::Warning {
                                stage: "deep".to_owned(),
                                item_path: Some(path_str.clone()),
                                message: format!("upsert_chunks failed: {e:#}"),
                                pressure: None,
                            },
                        );
                        hard_errors += 1;
                    }
                }
                // Persist the file's code-graph edges (imports/defines), keyed on the same
                // entry-path string as its chunks. Best-effort: a failure only warns.
                if !extracted.edges.is_empty() {
                    let edge_records: Vec<EdgeRecord> = extracted
                        .edges
                        .iter()
                        .map(|e| EdgeRecord {
                            from_path: path_str.clone(),
                            kind: e.kind.to_owned(),
                            to_ref: e.to.clone(),
                        })
                        .collect();
                    if let Err(e) = store.upsert_edges(&edge_records) {
                        push(
                            handle,
                            JobEvent::Warning {
                                stage: "deep".to_owned(),
                                item_path: Some(path_str.clone()),
                                message: format!("upsert_edges failed: {e:#}"),
                                pressure: None,
                            },
                        );
                    }
                }
            }
            done += 1;
        }

        // Update rolling throughput window (evict samples older than 5s).
        let now = std::time::Instant::now();
        let cutoff = now - std::time::Duration::from_secs(5);
        while samples.len() > 1 && samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            samples.pop_front();
        }
        samples.push_back((now, done));

        let (rate, eta) = if samples.len() >= 2 {
            let (oldest_t, oldest_done) = samples.front().unwrap();
            let elapsed = oldest_t.elapsed().as_secs_f64();
            let r = if elapsed > 0.0 {
                (done - oldest_done) as f64 / elapsed
            } else {
                0.0
            };
            let e = if r > 0.0 {
                (n_files - done) as f64 / r
            } else {
                0.0
            };
            (Some(r), Some(e))
        } else {
            (None, None)
        };

        push(
            handle,
            JobEvent::Progress {
                current: done,
                total: n_files,
                note: None,
                current_path: Some(path_str),
                items_per_sec: rate,
                eta_secs: eta,
            },
        );
    }

    // M5: if there were files to process but nothing was written and nothing was
    // already current, and at least one file hard-errored, the phase genuinely
    // failed — don't let the caller report "complete". (A folder of binary/empty
    // files that simply yields no chunks is NOT a failure and still returns true.)
    if !files.is_empty() && chunks_written == 0 && skipped == 0 && hard_errors > 0 {
        finalize_failed(
            handle,
            "deep",
            &anyhow::anyhow!(
                "no chunks were indexed — all {} file(s) failed to parse or store",
                files.len()
            ),
        );
        return false;
    }

    true
}

pub(crate) async fn run_summarize_phase(
    state: &AppState,
    path: &str,
    passes_override: Option<u32>,
    handle: &Arc<JobHandle>,
    // Optional (file_model, dir_model, num_ctx) from the "ask me first" popover;
    // when None, the configured describer models are used.
    model_override: Option<(String, String, u32)>,
) {
    push(
        handle,
        JobEvent::Start {
            kind: "summarize".into(),
            path: path.to_owned(),
            total: None,
        },
    );

    let db_path = (*state.db_path).clone();
    let mut cfg = state.config.describer.clone();
    let resource_cfg = state.config.resource.clone();
    let spec = state.machine_spec.clone();
    let headroom = resource_cfg.effective_headroom_bytes();
    let embedder = state.embedder.clone();
    let root = std::path::PathBuf::from(path);
    let (file_model, dir_model, num_ctx) = match &model_override {
        Some((f, d, n)) => (f.clone(), d.clone(), *n),
        None => (cfg.file_model.clone(), cfg.dir_model.clone(), cfg.num_ctx),
    };
    // Keep cfg truthful under an "ask me first" override: summary rows record
    // cfg.file_model/dir_model as their `model`, so a substituted model must be
    // reflected there too; model_fallback marks the substitution in provenance.
    cfg.model_fallback = file_model != cfg.file_model || dir_model != cfg.dir_model;
    cfg.file_model = file_model.clone();
    cfg.dir_model = dir_model.clone();
    cfg.num_ctx = num_ctx;
    // Route through the factory so `provider = "claude-code"` (the user's Claude
    // subscription) is honored here too, not just on the CLI summarize path.
    let describer = match indexa_llm::describer_from_config(
        &cfg.provider,
        &file_model,
        &dir_model,
        &cfg.base_url,
        num_ctx,
        &cfg.claude_bin,
    ) {
        Ok(d) => d,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // Memory watchdog: checked before each LLM summarization call.
    let mut wdog = WatchdogState::new();

    // Open a dedicated Store connection so we can hold it across async LLM awaits
    // without poisoning the shared mutex-wrapped store used by API handlers.
    let mut job_store = match indexa_core::store::Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // Force-requeue the whole subtree: reset any existing `done`/`failed` rows back
    // to `pending` so Regenerate actually re-runs the AI, not just drains new items.
    // `mark_for_resummary` (used internally) leaves `in_flight` rows untouched so
    // concurrent workers aren't double-claimed.
    let newly_enqueued = match requeue_subtree(&mut job_store, &root) {
        Ok(n) => n,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // Use the actual pending queue depth (includes items from other subtrees that
    // were already pending) so the progress "N / total" ETA is meaningful.
    let enqueued = if newly_enqueued > 0 {
        newly_enqueued
    } else {
        job_store
            .queue_stats()
            .map(|s| s.pending.max(0) as usize)
            .unwrap_or(0)
    };

    push(
        handle,
        JobEvent::Snapshot {
            count: enqueued as u64,
            bytes: 0,
        },
    );

    let mut done = 0usize;
    let mut errors = 0usize;
    let mut samples: std::collections::VecDeque<(std::time::Instant, u64)> =
        std::collections::VecDeque::with_capacity(16);
    samples.push_back((std::time::Instant::now(), 0));

    // Count per-directory defers (children not summarized yet) for the force-rollup cap.
    let mut defers: HashMap<String, u32> = HashMap::new();

    loop {
        // Honor cancellation requested via DELETE /api/jobs/:id.
        if handle.is_cancelled() {
            finalize_cancelled(handle, done);
            return;
        }

        let item = match job_store.next_queue_item() {
            Ok(Some(i)) => i,
            Ok(None) => break,
            Err(e) => {
                finalize_failed(handle, "summarize", &e);
                return;
            }
        };
        let item_path = item.path.clone();
        // Force the roll-up after too many defers so a stuck child can't hang the job.
        let force =
            item.kind == "dir" && defers.get(&item.path).copied().unwrap_or(0) >= MAX_DIR_DEFERS;

        // Watchdog: pause if memory is tight before the LLM summarization call. On a Critical
        // pause we unload the describer LLM and embedder so their RAM frees and we can resume.
        run_watchdog_check(
            &mut wdog,
            &spec,
            headroom,
            handle,
            "summarize",
            Some(embedder.as_ref()),
            Some(&*describer),
        )
        .await;

        let llm_start = std::time::Instant::now();

        // Only stream tokens when someone is watching (receiver_count > 0).
        // This avoids flooding the broadcast channel when no client is connected.
        let r = if handle.tx.receiver_count() > 0 {
            let h = handle.clone();
            let ip = item_path.clone();
            let model_name = if item.kind == "file" {
                cfg.file_model.clone()
            } else {
                cfg.dir_model.clone()
            };
            let stage = if item.kind == "file" {
                "summarize_file".to_owned()
            } else {
                "summarize_dir".to_owned()
            };
            let mut on_frag = move |frag: String| {
                broadcast_only(
                    &h,
                    JobEvent::LlmFragment {
                        item_path: ip.clone(),
                        model: model_name.clone(),
                        stage: stage.clone(),
                        fragment: frag,
                    },
                );
            };
            process_queue_item_with_passes(
                &mut job_store,
                describer.as_ref(),
                embedder.as_ref(),
                &item,
                &cfg,
                passes_override,
                Some(&mut on_frag),
                force,
            )
            .await
        } else {
            process_queue_item_with_passes(
                &mut job_store,
                describer.as_ref(),
                embedder.as_ref(),
                &item,
                &cfg,
                passes_override,
                None,
                force,
            )
            .await
        };
        let llm_secs = llm_start.elapsed().as_secs_f64();
        match r {
            Ok(QueueOutcome::Completed) => {
                defers.remove(&item.path);
                done += 1;
            }
            Ok(QueueOutcome::Failed) => {
                defers.remove(&item.path);
                errors += 1;
            }
            // Dir children not summarized yet; it was re-enqueued `pending`. Back off and
            // poll again (a deferred dir stays pending, so `break`-on-None won't drop it).
            Ok(QueueOutcome::Deferred) => {
                *defers.entry(item.path.clone()).or_insert(0) += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
            // Err = unexpected store error that left the row `in_flight`. Terminalize it
            // (best-effort) so it can't get stuck blocking the queue.
            Err(e) => {
                defers.remove(&item.path);
                errors += 1;
                if let Err(mark_err) =
                    job_store.mark_queue_state(&item.path, "failed", Some(&format!("{e:#}")))
                {
                    tracing::warn!(
                        path = %item.path,
                        error = %mark_err,
                        "summarize: failed to terminalize stuck queue row as failed; it may stay in_flight"
                    );
                }
            }
        }

        let processed = (done + errors) as u64;
        let now = std::time::Instant::now();
        let cutoff = now - std::time::Duration::from_secs(5);
        while samples.len() > 1 && samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            samples.pop_front();
        }
        samples.push_back((now, processed));

        let (rate, eta) = if samples.len() >= 2 {
            let (oldest_t, oldest_done) = samples.front().unwrap();
            let elapsed = oldest_t.elapsed().as_secs_f64();
            let r = if elapsed > 0.0 {
                (processed - oldest_done) as f64 / elapsed
            } else {
                0.0
            };
            // saturating_sub guards against processed > enqueued (the pending count
            // can drift if items went in-flight between snapshot and processing).
            let e = if r > 0.0 {
                (enqueued as u64).saturating_sub(processed) as f64 / r
            } else {
                0.0
            };
            (Some(r), Some(e))
        } else {
            (None, None)
        };

        let note = Some(format!("{:.1}s · {}", llm_secs, cfg.file_model));
        push(
            handle,
            JobEvent::Progress {
                current: processed,
                total: enqueued as u64,
                note,
                current_path: Some(item_path),
                items_per_sec: rate,
                eta_secs: eta,
            },
        );
    }

    push(
        handle,
        JobEvent::Done {
            summary: format!("{done} summaries generated"),
        },
    );
    handle.set_status(JobStatus::Done);
}
