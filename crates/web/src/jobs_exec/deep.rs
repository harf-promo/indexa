//! The `deep` phase: parse → chunk → embed every file (plus image-caption / audio-transcribe
//! / OCR / video-frame sub-passes), with the memory watchdog throttling between heavy steps.
//! The single largest job body; extracted from `jobs_exec` (v0.61) — pure move, no behavior change.

use super::watchdog::run_watchdog_check;
use super::{finalize_cancelled, finalize_done, finalize_failed, walk_for_job};
use crate::jobs::{broadcast_only, push, JobEvent, JobHandle};
use crate::AppState;
use indexa_core::{
    resource::{MachineSpec, WatchdogState},
    store::{chunk_content_hash, ChunkRecord, EdgeRecord},
    walker::EntryKind,
};
use indexa_embed::{AddOutcome, Completed, MissBatcher};
use indexa_llm::{Describer, OllamaLlm};
use indexa_query::contextual::{build_doc_context, contextual_embed_texts, ContextualEvent};
use std::sync::Arc;

/// Per-file payload the cross-file embed accumulator ([`MissBatcher`]) holds until every one
/// of the file's cache-miss chunks has been embedded, then hands back so the deep loop builds
/// + upserts its chunk records exactly once.
struct WebFileMeta {
    path_str: String,
    chunks: Vec<indexa_parsers::types::Chunk>,
    chunk_hashes: Vec<String>,
    edges: Vec<indexa_parsers::types::Edge>,
}

/// Build the file's chunk records (secret-redacted for storage) + upsert them and its
/// code-graph edges, updating the success/error counters. Shared by the zero-miss fast path
/// and the batched-embed finalize path. Store lock is held only across the two synchronous
/// upserts (no `.await` inside), so it never blocks a concurrent reader for long.
#[allow(clippy::too_many_arguments)]
async fn finalize_web_file(
    state: &AppState,
    handle: &Arc<JobHandle>,
    embed_model: &str,
    path_str: &str,
    chunks: &[indexa_parsers::types::Chunk],
    chunk_hashes: &[String],
    edges: &[indexa_parsers::types::Edge],
    embeddings: Vec<Option<Vec<f32>>>,
    chunks_written: &mut u64,
    hard_errors: &mut u64,
) {
    let mut chunk_records = Vec::with_capacity(chunks.len());
    for ((chunk, embedding), hash) in chunks.iter().zip(embeddings).zip(chunk_hashes) {
        chunk_records.push(ChunkRecord {
            entry_path: path_str.to_string(),
            seq: chunk.seq,
            heading: chunk.heading.clone(),
            // Redact secrets before storing (embed uses original text); shared choke point so
            // web deep honors [scan] redact_at_index like the CLI.
            text: indexa_query::redact::chunk_text_for_store(
                &chunk.text,
                state.config.scan.redact_at_index,
            ),
            language: chunk.language.clone(),
            embedding,
            embed_model: Some(embed_model.to_string()),
            content_hash: Some(hash.clone()),
        });
    }
    let mut store = state.store.lock().await;
    match store.upsert_chunks(&chunk_records) {
        Ok(()) => *chunks_written += chunk_records.len() as u64,
        Err(e) => {
            push(
                handle,
                JobEvent::Warning {
                    stage: "deep".to_owned(),
                    item_path: Some(path_str.to_string()),
                    message: format!("upsert_chunks failed: {e:#}"),
                    pressure: None,
                },
            );
            *hard_errors += 1;
        }
    }
    if !edges.is_empty() {
        let edge_records: Vec<EdgeRecord> = edges
            .iter()
            .map(|e| EdgeRecord {
                from_path: path_str.to_string(),
                kind: e.kind.to_owned(),
                to_ref: e.to.clone(),
            })
            .collect();
        if let Err(e) = store.upsert_edges(&edge_records) {
            push(
                handle,
                JobEvent::Warning {
                    stage: "deep".to_owned(),
                    item_path: Some(path_str.to_string()),
                    message: format!("upsert_edges failed: {e:#}"),
                    pressure: None,
                },
            );
        }
    }
}

/// Emit the per-file embed warnings (dim mismatch / embed failure) for a finalized file. The
/// counts are re-attributed to their owning file by the accumulator even though a flush mixes
/// files. Mirrors the pre-batching web warnings (dim-nulled vectors count toward the
/// "failed to embed" total, matching the old post-`enforce_embedding_dim` count).
fn emit_web_embed_warnings(
    handle: &Arc<JobHandle>,
    c: &Completed<WebFileMeta>,
    configured_dim: usize,
) {
    if c.dim_mismatch > 0 {
        push(
            handle,
            JobEvent::Warning {
                stage: "deep".to_owned(),
                item_path: Some(c.meta.path_str.clone()),
                message: format!(
                    "{} chunk(s) embedded at dim {} ≠ configured {} — stored text-only; \
                     fix [embedding] model/dim and re-run deep",
                    c.dim_mismatch,
                    c.dim_sample.unwrap_or(0),
                    configured_dim
                ),
                pressure: None,
            },
        );
    }
    let embed_failures = c.raw_failures + c.dim_mismatch;
    if embed_failures > 0 {
        push(
            handle,
            JobEvent::Warning {
                stage: "deep".to_owned(),
                item_path: Some(c.meta.path_str.clone()),
                message: format!("{embed_failures}/{} chunks failed to embed", c.miss_count),
                pressure: None,
            },
        );
    }
}

/// Embed everything buffered in the accumulator (one `embed_all`, internally sub-batched at
/// `EMBED_BATCH_SIZE`) and finalize each file whose misses are now resolved. The memory
/// watchdog runs first — while the buffer is full — so a Critical-pressure unload of the
/// embedder/LLM precedes the big batched embed. Used for the mid-loop flush (buffer full),
/// the cancel-drain, and the end-of-run tail flush.
#[allow(clippy::too_many_arguments)]
async fn flush_web_batch(
    batcher: &mut MissBatcher<WebFileMeta>,
    state: &AppState,
    handle: &Arc<JobHandle>,
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    ctx_llm: Option<&(dyn Describer + Send + Sync)>,
    embed_model: &str,
    chunks_written: &mut u64,
    hard_errors: &mut u64,
) {
    run_watchdog_check(
        wdog,
        spec,
        headroom,
        handle,
        "deep",
        Some(state.embedder.as_ref()),
        ctx_llm,
    )
    .await;
    let refs = batcher.batch_refs();
    let out = indexa_embed::embed_all(
        state.embedder.as_ref(),
        &refs,
        indexa_embed::EMBED_BATCH_SIZE,
    )
    .await;
    drop(refs);
    for c in batcher.scatter(out) {
        emit_web_embed_warnings(handle, &c, state.config.embedding.dim);
        finalize_web_file(
            state,
            handle,
            embed_model,
            &c.meta.path_str,
            &c.meta.chunks,
            &c.meta.chunk_hashes,
            &c.meta.edges,
            c.embeddings,
            chunks_written,
            hard_errors,
        )
        .await;
    }
}

/// Standalone deep: walks, deep-indexes, then finalises the job as done.
pub(crate) async fn run_deep_phase_standalone(
    state: &AppState,
    path: &str,
    handle: &Arc<JobHandle>,
) {
    let Some(entries) = walk_for_job(
        path,
        handle,
        &state.walk_semaphore,
        super::scan_walk_config(&state.config.scan),
    )
    .await
    else {
        return;
    };
    let n_files = entries.iter().filter(|e| e.kind == EntryKind::File).count();
    if run_deep_phase(state, path, &entries, handle).await {
        finalize_done(handle, &format!("Deep index complete: {n_files} files"));
    }
}

/// The deep-index phase: parse → chunk → embed every file (with image-caption / audio-transcribe
/// / OCR / video-frame sub-passes), throttled between heavy steps by the memory watchdog.
/// Returns `true` on success; `false` when it finalised the job itself (cancellation or error).
pub(crate) async fn run_deep_phase(
    state: &AppState,
    path: &str,
    entries: &[indexa_core::walker::Entry],
    handle: &Arc<JobHandle>,
) -> bool {
    // Secret files (`.env`, keys, `.pem`/keystores) are recorded by scan but not embedded unless
    // `[scan] include_sensitive` — redaction can't scrub a raw key, so their contents stay out of
    // the searchable index by default. Mirrors the CLI deep + watch (`should_index_file`) gates.
    let include_sensitive = state.config.scan.include_sensitive;
    let files: Vec<_> = entries
        .iter()
        .filter(|e| {
            e.kind == EntryKind::File
                && !e.is_binary
                && (include_sensitive
                    || !e.hint.as_ref().is_some_and(|h| {
                        h.deep_scan == indexa_core::surface::DeepScanPolicy::Sensitive
                    }))
        })
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
    // Optional PDF OCR (opt-in): pdftoppm + tesseract for scanned PDFs with no text layer.
    let ocr_enabled = state.config.parsers.pdf.ocr_enabled();
    let ocr_binary = state.config.parsers.pdf.ocr_binary().to_owned();
    let ocr_lang = state.config.parsers.pdf.ocr_lang.clone();
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
    // Chunk-aware registry honoring `[chunking]` size/overlap. This loop spawns a fresh blocking
    // task per file, so share one registry via `Arc` (cloned into each closure) rather than
    // rebuilding a default one per file (which the free `parse_guarded` would do).
    let registry = std::sync::Arc::new(indexa_parsers::registry::Registry::with_chunk(
        indexa_parsers::types::ChunkParams {
            size: state.config.chunking.size,
            overlap: state.config.chunking.overlap,
        },
    ));

    // Accumulate cache-miss embed-texts across files so each embed round-trip carries a full
    // batch instead of one file's 1–3 chunks. Files upsert as their misses resolve (in a
    // flush); the tail flush after the loop drains the rest.
    let mut batcher: MissBatcher<WebFileMeta> =
        MissBatcher::new(state.config.embedding.dim, indexa_embed::EMBED_BATCH_SIZE);

    for entry in &files {
        // Honor cancellation requested via DELETE /api/jobs/:id.
        if handle.is_cancelled() {
            // Flush what's already parsed + enriched — its (possibly LLM-costed) embed work
            // is paid for, so finalize it rather than discarding it, then report cancelled.
            if !batcher.is_empty() {
                flush_web_batch(
                    &mut batcher,
                    state,
                    handle,
                    &mut wdog,
                    &spec,
                    headroom,
                    ctx_llm
                        .as_ref()
                        .map(|l| l as &(dyn Describer + Send + Sync)),
                    &embed_model,
                    &mut chunks_written,
                    &mut hard_errors,
                )
                .await;
            }
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
            let reg = registry.clone();
            let mut extracted = match tokio::task::spawn_blocking(move || {
                reg.parse_guarded(&ep, sz, max_parse_bytes)
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

            // PDF OCR (opt-in): a scanned PDF with no text layer is rasterised + OCR'd and the
            // recognised text appended as a chunk. Blocking subprocess → spawn_blocking; fails open.
            if ocr_enabled && extracted.mime == "application/pdf" {
                let layer_words: usize = extracted
                    .chunks
                    .iter()
                    .map(|c| c.text.split_whitespace().count())
                    .sum();
                if layer_words < 10 {
                    let bin = ocr_binary.clone();
                    let lang = ocr_lang.clone();
                    let p = entry.path.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        indexa_parsers::pdf::ocr_pdf(&p, &bin, lang.as_deref())
                    })
                    .await;
                    match res {
                        Ok(Ok(text)) if !text.trim().is_empty() => {
                            let seq = extracted.chunks.len();
                            extracted.chunks.push(indexa_parsers::types::Chunk {
                                source: entry.path.clone(),
                                seq,
                                heading: "ocr".to_owned(),
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
                                message: format!("OCR failed: {e:#}"),
                                pressure: None,
                            },
                        ),
                        Err(e) => push(
                            handle,
                            JobEvent::Warning {
                                stage: "deep".to_owned(),
                                item_path: Some(path_str.clone()),
                                message: format!("OCR task panicked: {e}"),
                                pressure: None,
                            },
                        ),
                    }
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
                // Compute SHA-256 of each chunk's raw text for embedding cache lookup.
                // Hash is over the ORIGINAL text (not enriched blurb) so it stays valid
                // across contextual-retrieval runs on the same source.
                let chunk_hashes: Vec<String> = extracted
                    .chunks
                    .iter()
                    .map(|c| chunk_content_hash(&c.text))
                    .collect();

                // Load cached embeddings for this file (hash → Vec<f32>). Fail-open.
                let hash_cache = {
                    let store = state.store.lock().await;
                    store
                        .cached_embeddings_by_hash(&path_str)
                        .unwrap_or_default()
                };

                // Partition into cache-hits and misses.
                let mut cache_hits: Vec<Option<Vec<f32>>> = vec![None; extracted.chunks.len()];
                let mut miss_indices: Vec<usize> = Vec::new();
                for (i, hash) in chunk_hashes.iter().enumerate() {
                    if let Some(v) = hash_cache.get(hash) {
                        cache_hits[i] = Some(v.clone());
                    } else {
                        miss_indices.push(i);
                    }
                }

                // Build a document-level context string for contextual retrieval.
                // Uses the shared `build_doc_context` helper (single source of truth).
                // Built from the full file regardless of which chunks are misses.
                let doc_context: Option<String> = if ctx_llm.is_some() {
                    let texts: Vec<&str> =
                        extracted.chunks.iter().map(|c| c.text.as_str()).collect();
                    Some(build_doc_context(&texts))
                } else {
                    None
                };

                // Phase 1 — materialize embed text for cache-miss chunks only. With contextual
                // retrieval enabled, each miss chunk gets a situating blurb; otherwise the embed
                // text is just the chunk text.
                let miss_raw_texts: Vec<&str> = miss_indices
                    .iter()
                    .map(|&i| extracted.chunks[i].text.as_str())
                    .collect();
                let miss_embed_texts: Vec<String> = if !miss_raw_texts.is_empty() {
                    if let (Some(ref llm), Some(ref doc)) = (&ctx_llm, &doc_context) {
                        let ps = path_str.clone();
                        let model_name = cfg.file_model.clone();
                        let h = handle.clone();
                        contextual_embed_texts(
                            llm,
                            doc,
                            &miss_raw_texts,
                            None,
                            &path_str,
                            move |event| match event {
                                ContextualEvent::BlurbFragment { fragment, .. } => {
                                    broadcast_only(
                                        &h,
                                        JobEvent::LlmFragment {
                                            item_path: ps.clone(),
                                            model: model_name.clone(),
                                            stage: "context_blurb".to_owned(),
                                            fragment,
                                        },
                                    );
                                }
                                ContextualEvent::BlurbFailed { error, .. } => {
                                    push(
                                        &h,
                                        JobEvent::Warning {
                                            stage: "deep".to_owned(),
                                            item_path: Some(ps.clone()),
                                            message: format!("context blurb failed: {error:#}"),
                                            pressure: None,
                                        },
                                    );
                                }
                            },
                        )
                        .await
                    } else if cfg.contextual_prefix {
                        // Deterministic, local, no-LLM contextual prefix (mirrors the CLI deep
                        // path). Prepend the file path, section heading, and a doc-context snippet
                        // to each miss chunk's embed input; the stored/hashed text is untouched.
                        let all_raw: Vec<&str> =
                            extracted.chunks.iter().map(|c| c.text.as_str()).collect();
                        let doc_ctx = build_doc_context(&all_raw);
                        let miss_headings: Vec<&str> = miss_indices
                            .iter()
                            .map(|&i| extracted.chunks[i].heading.as_str())
                            .collect();
                        indexa_query::contextual::contextual_prefix_texts(
                            &doc_ctx,
                            &miss_headings,
                            &miss_raw_texts,
                            &path_str,
                        )
                    } else {
                        miss_raw_texts.iter().map(|s| s.to_string()).collect()
                    }
                } else {
                    Vec::new()
                };

                // Tag each miss with its chunk slot and hand the file to the accumulator,
                // moving its chunks/hashes/edges in. The accumulator batches embeds across
                // files and returns the file (to finalize) once all its misses resolve; a
                // zero-miss file (all cache hits) completes immediately with no embed
                // round-trip. The memory watchdog runs inside the flush, right before the
                // batched embed — so a Critical-pressure unload precedes the big embed.
                let miss_texts: Vec<(usize, String)> =
                    miss_indices.into_iter().zip(miss_embed_texts).collect();
                let meta = WebFileMeta {
                    path_str: path_str.clone(),
                    chunks: std::mem::take(&mut extracted.chunks),
                    chunk_hashes,
                    edges: std::mem::take(&mut extracted.edges),
                };
                if let AddOutcome::Complete(c) = batcher.add_file(cache_hits, miss_texts, meta) {
                    finalize_web_file(
                        state,
                        handle,
                        &embed_model,
                        &c.meta.path_str,
                        &c.meta.chunks,
                        &c.meta.chunk_hashes,
                        &c.meta.edges,
                        c.embeddings,
                        &mut chunks_written,
                        &mut hard_errors,
                    )
                    .await;
                }

                if batcher.is_full() {
                    flush_web_batch(
                        &mut batcher,
                        state,
                        handle,
                        &mut wdog,
                        &spec,
                        headroom,
                        ctx_llm
                            .as_ref()
                            .map(|l| l as &(dyn Describer + Send + Sync)),
                        &embed_model,
                        &mut chunks_written,
                        &mut hard_errors,
                    )
                    .await;
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

        let (rate, eta) = super::throughput_eta(&samples, done, n_files);

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

    // Tail flush: embed any files still buffered below the batch threshold, then finalize
    // them (upsert + edges). Their chunks count toward `chunks_written` here.
    if !batcher.is_empty() {
        flush_web_batch(
            &mut batcher,
            state,
            handle,
            &mut wdog,
            &spec,
            headroom,
            ctx_llm
                .as_ref()
                .map(|l| l as &(dyn Describer + Send + Sync)),
            &embed_model,
            &mut chunks_written,
            &mut hard_errors,
        )
        .await;
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
