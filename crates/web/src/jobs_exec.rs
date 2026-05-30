//! Background job execution bodies — the long `run_*` scan/deep/summarize logic
//! plus their finalization and watchdog helpers. Spawned by the `api_job_*`
//! handlers; no axum routing lives here.

use crate::jobs::{broadcast_only, push, JobEvent, JobHandle, JobStatus, Jobs};
use crate::AppState;
use indexa_core::{
    resource::{
        assess, pause_decision, MachineSpec, PauseAction, Pressure, WatchdogState, MAX_PAUSE_SECS,
    },
    store::ChunkRecord,
    walker::{walk, EntryKind, WalkConfig},
};
use indexa_llm::{Generator, OllamaLlm};
use indexa_query::{enqueue_subtree, process_queue_item_with_passes};
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
    *handle.status.lock().unwrap() = JobStatus::Failed;
}

fn finalize_done(handle: &Arc<JobHandle>, summary: &str) {
    push(
        handle,
        JobEvent::Done {
            summary: summary.to_owned(),
        },
    );
    *handle.status.lock().unwrap() = JobStatus::Done;
}

/// Emit a terminal Done event noting the job was cancelled mid-run.
fn finalize_cancelled(handle: &Arc<JobHandle>, done: usize) {
    push(
        handle,
        JobEvent::Done {
            summary: format!("Cancelled after {done} items"),
        },
    );
    *handle.status.lock().unwrap() = JobStatus::Done;
}

/// Check memory pressure before an Ollama call.
///
/// If pressure is Throttle or Critical:
///   1. Emits a Warning event so the user can see it in the Jobs UI.
///   2. Sleeps in a loop until pressure returns to Ok.
///
/// The caller should invoke this before every embedding or LLM call
/// in the hot loops of `run_deep_phase` and `run_summarize_phase`.
async fn run_watchdog_check(
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    handle: &Arc<JobHandle>,
    stage: &str,
) {
    let sample = wdog.sample();
    let pressure = assess(&sample, spec, headroom);
    if pressure == Pressure::Ok {
        return;
    }

    let level = if pressure == Pressure::Critical {
        "critical"
    } else {
        "high"
    };
    let swap_gb = sample.swap_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let swap_pct = sample
        .swap_used_bytes
        .checked_mul(100)
        .and_then(|v| v.checked_div(sample.swap_total_bytes))
        .unwrap_or(100);
    push(
        handle,
        JobEvent::Warning {
            stage: stage.to_owned(),
            item_path: None,
            message: format!(
                "Memory pressure {level} — swap at {swap_pct}% ({swap_gb:.1} GB used). \
                 Pausing to avoid freeze. Job will resume automatically."
            ),
        },
    );

    // Wait until pressure clears, capped at resource::MAX_PAUSE_SECS. The shared
    // `pause_decision` is re-evaluated against a fresh sample each tick, so an escalation
    // (Throttle → Critical) immediately tightens the cadence — the old loop fixed the
    // interval from the pre-loop level and capped Throttle at only 2 minutes.
    let mut elapsed = 0u64;
    let mut next_status_at = 30u64;
    loop {
        let s = wdog.sample();
        match pause_decision(assess(&s, spec, headroom), elapsed) {
            PauseAction::Resume => break,
            PauseAction::Proceed => {
                push(
                    handle,
                    JobEvent::Warning {
                        stage: stage.to_owned(),
                        item_path: None,
                        message: format!(
                            "Memory pressure did not clear after {MAX_PAUSE_SECS} s — \
                             proceeding anyway. Consider closing other apps or setting a \
                             lower headroom in [resource] config."
                        ),
                    },
                );
                break;
            }
            PauseAction::Sleep(secs) => {
                // Emit a follow-up roughly every 30 s so the user isn't left wondering.
                if elapsed >= next_status_at {
                    let swap_gb = s.swap_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let swap_pct = s
                        .swap_used_bytes
                        .checked_mul(100)
                        .and_then(|v| v.checked_div(s.swap_total_bytes))
                        .unwrap_or(100);
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: stage.to_owned(),
                            item_path: None,
                            message: format!(
                                "Still waiting for swap to clear (swap: {swap_pct}% / \
                                 {swap_gb:.1} GB) — {elapsed}/{MAX_PAUSE_SECS} s …"
                            ),
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

pub(crate) async fn run_index_job(state: AppState, path: String, handle: Arc<JobHandle>) {
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
    run_summarize_phase(&state, &path, None, &handle).await;
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
        Some(OllamaLlm::new(&base_url, &cfg.file_model))
    } else {
        None
    };

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

        let is_current = {
            let store = state.store.lock().await;
            store.chunks_are_current(&path_str).unwrap_or(false)
        };
        if is_current {
            skipped += 1;
            done += 1;
        } else {
            let ep = entry.path.clone();
            let sz = entry.size;
            let extracted = match tokio::task::spawn_blocking(move || {
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
                        },
                    );
                    hard_errors += 1;
                    done += 1;
                    continue;
                }
            };

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

                let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
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
                                        },
                                    );
                                    chunk.text.clone()
                                }
                            }
                        } else {
                            chunk.text.clone()
                        };

                    // Watchdog: pause if memory is tight before the embed call.
                    run_watchdog_check(&mut wdog, &spec, headroom, handle, "deep").await;

                    let embedding = match state.embedder.embed(&embed_text).await {
                        Ok(v) => Some(v),
                        Err(e) => {
                            push(
                                handle,
                                JobEvent::Warning {
                                    stage: "deep".to_owned(),
                                    item_path: Some(path_str.clone()),
                                    message: format!("embed failed: {e:#}"),
                                },
                            );
                            None
                        }
                    };
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
                            },
                        );
                        hard_errors += 1;
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
    let cfg = state.config.describer.clone();
    let resource_cfg = state.config.resource.clone();
    let spec = state.machine_spec.clone();
    let headroom = resource_cfg.effective_headroom_bytes();
    let embedder = state.embedder.clone();
    let root = std::path::PathBuf::from(path);
    let base_url = OllamaLlm::resolve_base_url(Some(&cfg.base_url));
    let describer = OllamaLlm::new_with_dir_model(&base_url, &cfg.file_model, &cfg.dir_model);

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

    let newly_enqueued = match enqueue_subtree(&mut job_store, &root) {
        Ok(n) => n,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // The work total is the actual pending queue depth, not just the items WE
    // enqueued: re-running summarize on an already-queued path enqueues 0 new
    // items but still drains the existing backlog. Using `newly_enqueued` (0)
    // as the total produced "4 / 0" progress and a garbage ETA. Fall back to
    // the real pending count when nothing new was enqueued.
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

        // Watchdog: pause if memory is tight before the LLM summarization call.
        run_watchdog_check(&mut wdog, &spec, headroom, handle, "summarize").await;

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
                &describer,
                embedder.as_ref(),
                &item,
                &cfg,
                passes_override,
                Some(&mut on_frag),
            )
            .await
        } else {
            process_queue_item_with_passes(
                &mut job_store,
                &describer,
                embedder.as_ref(),
                &item,
                &cfg,
                passes_override,
                None,
            )
            .await
        };
        let llm_secs = llm_start.elapsed().as_secs_f64();
        match r {
            Ok(true) => done += 1,
            // Ok(false) = item failed but was recorded in the queue; Err = unexpected store error.
            Ok(false) | Err(_) => errors += 1,
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
    *handle.status.lock().unwrap() = JobStatus::Done;
}
