//! The `deep` phase: parse → chunk → embed every file (plus image-caption / audio-transcribe
//! / OCR / video-frame sub-passes), with the memory watchdog throttling between heavy steps.
//! The single largest job body; extracted from `jobs_exec` (v0.61) — pure move, no behavior change.

use super::watchdog::run_watchdog_check;
use super::{finalize_cancelled, finalize_done, finalize_failed, walk_for_job};
use crate::jobs::{broadcast_only, push, JobEvent, JobHandle};
use crate::AppState;
use indexa_core::{
    config::SummaryMode,
    resource::{MachineSpec, WatchdogState},
    store::{chunk_content_hash, ChunkRecord, EdgeRecord, SymbolRecord},
    walker::EntryKind,
};
use indexa_embed::{AddOutcome, MissBatcher};
use indexa_llm::{Describer, OllamaLlm};
use indexa_query::contextual::{build_doc_context, contextual_embed_texts, ContextualEvent};
use std::sync::Arc;

/// Per-file payload buffered in the cross-file [`MissBatcher`] between registration and the
/// flush that resolves its cache-miss embeddings — everything [`persist_completed_file`] needs
/// that isn't already captured in the resolved `embeddings` vector. Closes #367: previously
/// every file with ≥1 cache-miss chunk issued its own `embed_all` round-trip; batching these
/// across files cuts a deep job's HTTP round-trips roughly `EMBED_BATCH_SIZE`-fold, without
/// changing what gets stored (see `crates/embed/src/batcher.rs`'s correctness note).
struct PendingFile {
    entry: indexa_core::walker::Entry,
    path_str: String,
    chunks: Vec<indexa_parsers::types::Chunk>,
    chunk_hashes: Vec<String>,
    edges: Vec<indexa_parsers::types::Edge>,
}

/// Push the same dim-mismatch / embed-failure warnings the old per-file path pushed, now
/// driven off a `Completed`'s aggregated counts. `raw_failures` and `dim_mismatch` are tracked
/// separately by `MissBatcher::scatter`; the old code's `embed_failures` count (computed AFTER
/// `enforce_embedding_dim` nulled mismatched slots) is exactly their sum, so summing here
/// reproduces the same pushed messages — including that the failure warning fires
/// unconditionally (unlike the CLI path, which suppresses it when there's already a dim-
/// mismatch warning for the same file).
fn warn_embed_issues(
    handle: &Arc<JobHandle>,
    path_str: &str,
    dim_mismatch: usize,
    dim_sample: Option<usize>,
    raw_failures: usize,
    miss_count: usize,
    configured_dim: usize,
) {
    if dim_mismatch > 0 {
        push(
            handle,
            JobEvent::Warning {
                stage: "deep".to_owned(),
                item_path: Some(path_str.to_owned()),
                message: format!(
                    "{dim_mismatch} chunk(s) embedded at dim {} ≠ configured {} — stored \
                     text-only; fix [embedding] model/dim and re-run deep",
                    dim_sample.unwrap_or(0),
                    configured_dim
                ),
                pressure: None,
            },
        );
    }
    let embed_failures = raw_failures + dim_mismatch;
    if embed_failures > 0 {
        push(
            handle,
            JobEvent::Warning {
                stage: "deep".to_owned(),
                item_path: Some(path_str.to_owned()),
                message: format!("{embed_failures}/{miss_count} chunks failed to embed"),
                pressure: None,
            },
        );
    }
}

/// Advance job progress by exactly one file: bumps `done`, updates the rolling throughput
/// window, and pushes the `Progress` event. Callers MUST only call this once a file's data is
/// actually persisted — for a synchronously-completed file (already-current skip, hard error,
/// `summaries-only`, a zero-miss/`AddOutcome::Complete` batcher result) that's right where it
/// completes; for a file the cross-file [`MissBatcher`] buffered (`AddOutcome::Buffered`), it's
/// deferred to [`flush_deep_batcher`]'s per-file loop, at the point that file's entries/chunks/
/// edges are actually written — never at buffer-registration time. Otherwise a crash between
/// registering a file's misses and the next flush would leave the last-observed progress
/// overstating what's actually in the database.
fn record_file_progress(
    handle: &Arc<JobHandle>,
    path_str: &str,
    done: &mut u64,
    samples: &mut std::collections::VecDeque<(std::time::Instant, u64)>,
    n_files: u64,
) {
    *done += 1;
    let now = std::time::Instant::now();
    let cutoff = now - std::time::Duration::from_secs(5);
    while samples.len() > 1 && samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
        samples.pop_front();
    }
    samples.push_back((now, *done));
    let (rate, eta) = super::throughput_eta(samples, *done, n_files);
    push(
        handle,
        JobEvent::Progress {
            current: *done,
            total: n_files,
            note: None,
            current_path: Some(path_str.to_owned()),
            items_per_sec: rate,
            eta_secs: eta,
        },
    );
}

/// Build chunk records from a fully-resolved `embeddings` vector and persist one file: entries,
/// then chunks (unless `skip_embed_work`), then best-effort edges/symbols — each store op
/// reported via a `Warning` event on failure rather than aborting the job (parity with the old
/// per-file error handling). Shared by the summaries-only fast path (embeddings all `None`,
/// resolved synchronously), a zero-miss `MissBatcher::add_file` completion, and a post-
/// `scatter` completion. Returns `(entries_written, chunks_written, hard_errors)` deltas for
/// the caller's running totals.
#[allow(clippy::too_many_arguments)] // one flat finalize; grouping would just move fields around
async fn persist_completed_file(
    state: &AppState,
    handle: &Arc<JobHandle>,
    entry: indexa_core::walker::Entry,
    path_str: &str,
    chunks: &[indexa_parsers::types::Chunk],
    chunk_hashes: Vec<String>,
    edges: &[indexa_parsers::types::Edge],
    embeddings: Vec<Option<Vec<f32>>>,
    embed_model: Option<&str>,
    skip_embed_work: bool,
) -> (u64, u64, u64) {
    let mut chunk_records = Vec::with_capacity(chunks.len());
    for ((chunk, embedding), hash) in chunks.iter().zip(embeddings).zip(chunk_hashes) {
        chunk_records.push(ChunkRecord {
            entry_path: path_str.to_owned(),
            seq: chunk.seq,
            heading: chunk.heading.clone(),
            // Redact secrets before storing (embed uses original text); shared choke
            // point so web deep honors [scan] redact_at_index like the CLI.
            text: indexa_query::redact::chunk_text_for_store(
                &chunk.text,
                state.config.scan.redact_at_index,
            ),
            language: chunk.language.clone(),
            embedding,
            embed_model: embed_model.map(|m| m.to_owned()),
            content_hash: Some(hash),
        });
    }

    let (mut entries_written, mut chunks_written, mut hard_errors) = (0u64, 0u64, 0u64);
    let mut store = state.store.lock().await;
    // The standalone Deep job can run without a preceding scan, so without this the file has
    // no `entries` row — its chunks would be orphans, silently deleted the next time
    // `prune_orphans` runs. Always written regardless of mode.
    match store.upsert_entries(&[entry]) {
        Ok(()) => entries_written += 1,
        Err(e) => {
            push(
                handle,
                JobEvent::Warning {
                    stage: "deep".to_owned(),
                    item_path: Some(path_str.to_owned()),
                    message: format!("upsert_entries failed: {e:#}"),
                    pressure: None,
                },
            );
            hard_errors += 1;
        }
    }
    // `summaries-only` never persists chunk rows — that's the entire ~100× size win;
    // `summarize_file` re-parses the file itself when no chunks are stored.
    if !skip_embed_work {
        match store.upsert_chunks(&chunk_records) {
            Ok(()) => chunks_written += chunk_records.len() as u64,
            Err(e) => {
                push(
                    handle,
                    JobEvent::Warning {
                        stage: "deep".to_owned(),
                        item_path: Some(path_str.to_owned()),
                        message: format!("upsert_chunks failed: {e:#}"),
                        pressure: None,
                    },
                );
                hard_errors += 1;
            }
        }
    }
    // Persist the file's code-graph edges (imports/defines), keyed on the same entry-path
    // string as its chunks. Best-effort: a failure only warns.
    if !edges.is_empty() {
        let edge_records: Vec<EdgeRecord> = edges
            .iter()
            .map(|e| EdgeRecord {
                from_path: path_str.to_owned(),
                kind: e.kind.to_owned(),
                to_ref: e.to.clone(),
            })
            .collect();
        if let Err(e) = store.upsert_edges(&edge_records) {
            push(
                handle,
                JobEvent::Warning {
                    stage: "deep".to_owned(),
                    item_path: Some(path_str.to_owned()),
                    message: format!("upsert_edges failed: {e:#}"),
                    pressure: None,
                },
            );
        }
        // Symbols (2.1): kind + line range, extracted alongside `defines` edges.
        let symbol_records: Vec<SymbolRecord> = edges
            .iter()
            .filter(|e| e.kind == "defines")
            .filter_map(|e| {
                let (start, end) = e.line_range?;
                Some(SymbolRecord {
                    path: path_str.to_owned(),
                    name: e.to.clone(),
                    kind: e.symbol_kind.unwrap_or("other").to_owned(),
                    start_line: start as i64,
                    end_line: end as i64,
                })
            })
            .collect();
        if !symbol_records.is_empty() {
            if let Err(e) = store.upsert_symbols(&symbol_records) {
                push(
                    handle,
                    JobEvent::Warning {
                        stage: "deep".to_owned(),
                        item_path: Some(path_str.to_owned()),
                        message: format!("upsert_symbols failed: {e:#}"),
                        pressure: None,
                    },
                );
            }
        }
    }
    (entries_written, chunks_written, hard_errors)
}

/// Flush the batcher: memory-watchdog-gate, then embed every currently-buffered cross-file
/// miss in one (internally sub-batched) round-trip, scatter results back to owning files, and
/// persist every file that completes. This is now where the watchdog's "before each Ollama
/// call" check lives — the actual embed call moved here from the old per-file site, so the
/// check moves with it (still unloads the embedder/ctx LLM under Critical pressure, still
/// gates every batched embed round-trip). Called at `is_full()`, at end-of-run to drain a
/// final partial batch, and once more on cancellation to finalize already-enriched work rather
/// than discard it. Returns `(entries_written, chunks_written, hard_errors)` deltas.
///
/// Also where a batcher-buffered file's job progress is advanced (`done`/`samples`/the
/// `Progress` event, via [`record_file_progress`]) — NOT at buffer-registration time — since
/// this is the point each file's data is actually persisted.
#[allow(clippy::too_many_arguments)]
async fn flush_deep_batcher(
    batcher: &mut MissBatcher<PendingFile>,
    state: &AppState,
    handle: &Arc<JobHandle>,
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    ctx_llm: Option<&OllamaLlm>,
    embed_model: &str,
    done: &mut u64,
    samples: &mut std::collections::VecDeque<(std::time::Instant, u64)>,
    n_files: u64,
) -> (u64, u64, u64) {
    run_watchdog_check(
        wdog,
        spec,
        headroom,
        handle,
        "deep",
        Some(state.embedder.as_ref()),
        ctx_llm.map(|l| l as &(dyn Describer + Send + Sync)),
    )
    .await;

    let refs = batcher.batch_refs();
    let results = indexa_embed::embed_all(
        state.embedder.as_ref(),
        &refs,
        indexa_embed::EMBED_BATCH_SIZE,
    )
    .await;
    drop(refs);

    let (mut entries_written, mut chunks_written, mut hard_errors) = (0u64, 0u64, 0u64);
    for c in batcher.scatter(results) {
        warn_embed_issues(
            handle,
            &c.meta.path_str,
            c.dim_mismatch,
            c.dim_sample,
            c.raw_failures,
            c.miss_count,
            state.config.embedding.dim,
        );
        let PendingFile {
            entry,
            path_str,
            chunks,
            chunk_hashes,
            edges,
        } = c.meta;
        // Never `skip_embed_work` here — that path bypasses the batcher entirely and finalizes
        // inline instead (see `run_deep_phase`).
        let (e, c_, h) = persist_completed_file(
            state,
            handle,
            entry,
            &path_str,
            &chunks,
            chunk_hashes,
            &edges,
            c.embeddings,
            Some(embed_model),
            false,
        )
        .await;
        entries_written += e;
        chunks_written += c_;
        hard_errors += h;
        record_file_progress(handle, &path_str, done, samples, n_files);
    }
    (entries_written, chunks_written, hard_errors)
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

    // `summaries-only` never stores chunks (that's the ~100× size win), so every model call
    // that only exists to enrich a stored chunk — embeddings, contextual blurbs, image
    // captions, audio transcription, PDF OCR, video frame captions — is wasted work here.
    // Parsing + entries/edges/symbols still run: `summarize_file` re-parses the file itself
    // (`sample_via_parse` in `indexa_query::summarize`) when no chunks are stored, so this
    // phase doesn't need to feed it a sample — but that means the describer prompt comes from
    // the default 800/100-word registry, not this server's `[chunking]`/parser config.
    let summary_mode = cfg.mode.clone();
    let skip_embed_work = summary_mode == SummaryMode::SummariesOnly;

    // Build a contextual-retrieval LLM if the feature is enabled.
    let ctx_llm: Option<OllamaLlm> = if cfg.contextual_retrieval && !skip_embed_work {
        let base_url = OllamaLlm::resolve_base_url(Some(&cfg.base_url));
        Some(OllamaLlm::new(&base_url, &cfg.file_model).with_num_ctx(cfg.num_ctx))
    } else {
        None
    };

    // Optional video frame captioning (opt-in, v0.16).
    let video_caption = state.config.parsers.video.caption && !skip_embed_work;
    // Optional image captioning (opt-in): a vision model adds a caption chunk per image.
    // The same OllamaLlm handle drives BOTH image and video captioning, so build it when
    // EITHER is enabled — otherwise enabling only `video.caption` would silently no-op
    // (frames extracted, nothing captioned). The image caption model is used as the handle's
    // default; per-frame video calls pass `video_model` explicitly.
    let image_caption = state.config.parsers.image.caption && !skip_embed_work;
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
    let transcribe = state.config.parsers.audio.transcribe && !skip_embed_work;
    let transcribe_binary = state.config.parsers.audio.transcribe_binary().to_owned();
    let transcribe_model = state.config.parsers.audio.model.clone();
    // Optional PDF OCR (opt-in): pdftoppm + tesseract for scanned PDFs with no text layer.
    let ocr_enabled = state.config.parsers.pdf.ocr_enabled() && !skip_embed_work;
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
                                   // Entries successfully upserted — the success signal in `summaries-only` mode, where
                                   // `chunks_written` stays 0 by design (see the M5 check at the end of this function).
    let mut entries_written = 0u64;
    let mut hard_errors = 0u64; // parse/panic/upsert failures
                                // Rolling throughput: ring buffer of (instant, items_done) samples, last ~5s.
    let mut samples: std::collections::VecDeque<(std::time::Instant, u64)> =
        std::collections::VecDeque::with_capacity(16);
    samples.push_back((std::time::Instant::now(), 0));
    let max_parse_bytes = state.config.parsers.max_file_mb.saturating_mul(1024 * 1024);
    // Chunk-aware registry honoring `[chunking]` size/overlap. This loop spawns a fresh blocking
    // task per file, so share one registry via `Arc` (cloned into each closure) rather than
    // rebuilding a default one per file (which the free `parse_guarded` would do).
    let mut registry_inner =
        indexa_parsers::registry::Registry::with_chunk(indexa_parsers::types::ChunkParams {
            size: state.config.chunking.size,
            overlap: state.config.chunking.overlap,
            encoding: indexa_parsers::types::TextEncoding::from_config_str(
                &state.config.parsers.encoding,
            ),
        });
    registry_inner.register_preprocessors(&crate::preprocessor_specs(&state.config));
    if state.config.parsers.compressed {
        registry_inner.enable_compressed();
    }
    let registry = std::sync::Arc::new(registry_inner);

    // Accumulates cache-miss embed-texts across files so `embed_all` runs on full
    // `EMBED_BATCH_SIZE` batches instead of one file's 1–3 misses at a time (#367). Flushed at
    // `is_full()` below, at end-of-run, and (to avoid discarding already-paid-for parse/
    // contextual-LLM enrichment) once more on cancellation. Unused whenever `skip_embed_work`
    // is set, since that path finalizes inline without embedding at all.
    let mut batcher: MissBatcher<PendingFile> =
        MissBatcher::new(state.config.embedding.dim, indexa_embed::EMBED_BATCH_SIZE);

    for entry in &files {
        // Honor cancellation requested via DELETE /api/jobs/:id. Flush first: files already
        // buffered here were fully parsed (and, for contextual retrieval, already paid for an
        // LLM blurb call) — finalizing them rather than discarding keeps that work instead of
        // wasting it. The job is ending either way, so the flush's written/error counts (unlike
        // the same call's `is_full()`/tail-flush uses below) have nothing left to feed into.
        if handle.is_cancelled() {
            if !batcher.is_empty() {
                let _ = flush_deep_batcher(
                    &mut batcher,
                    state,
                    handle,
                    &mut wdog,
                    &spec,
                    headroom,
                    ctx_llm.as_ref(),
                    &embed_model,
                    &mut done,
                    &mut samples,
                    n_files,
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
        // NOTE: in `summaries-only` mode this check can never see a chunk row (none are ever
        // stored), so it always re-parses every file on every pass — cheap relative to
        // embedding, but the dominant cost once nothing is embedded. Known, not a bug; a real
        // fix needs a freshness marker independent of the chunks table.
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
            record_file_progress(handle, &path_str, &mut done, &mut samples, n_files);
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

                // `summaries-only` never persists the chunk at all, so computing an embedding
                // for it is wasted work — skip straight to an all-None vector, exactly like the
                // CLI's `--no-embed` path, and finalize inline (this mode never touches the
                // cross-file batcher). Otherwise, register with it below.
                if skip_embed_work {
                    let all_embeddings: Vec<Option<Vec<f32>>> = vec![None; extracted.chunks.len()];
                    let (e, c, h) = persist_completed_file(
                        state,
                        handle,
                        (**entry).clone(),
                        &path_str,
                        &extracted.chunks,
                        chunk_hashes,
                        &extracted.edges,
                        all_embeddings,
                        None,
                        true,
                    )
                    .await;
                    entries_written += e;
                    chunks_written += c;
                    hard_errors += h;
                    record_file_progress(handle, &path_str, &mut done, &mut samples, n_files);
                } else {
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

                    // Memory watchdog: checked before every file registers its misses with the
                    // cross-file batcher, not just at flush points (`is_full()`/end-of-run/
                    // cancellation) — batching the embed round-trips (#367/MissBatcher) must not
                    // widen the pause cadence: up to `EMBED_BATCH_SIZE` files' worth of parsed
                    // chunks/edges/LLM-enrichment could otherwise accumulate before a Critical-
                    // pressure pause is even checked. This restores the original per-file
                    // cadence; `flush_deep_batcher` checks again right before the actual (and
                    // possibly much-later) `embed_all` round-trip.
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

                    // Register this file's misses with the cross-file batcher instead of
                    // embedding them here directly (#367) — the batcher accumulates buffered
                    // embed-texts across files so the eventual `embed_all` call runs on a full
                    // batch rather than this one file's lone 1–3 misses. `add_file` finalizes a
                    // zero-miss file (`miss_indices` empty) synchronously and it never touches
                    // the buffer — this is also why a cache-hit chunk never "enters the
                    // batcher": only miss embed-texts are ever buffered; `cache_hits`' `Some`
                    // slots ride along in the `embeddings` vector and are returned untouched.
                    let miss_texts: Vec<(usize, String)> =
                        miss_indices.into_iter().zip(miss_embed_texts).collect();
                    let meta = PendingFile {
                        entry: (**entry).clone(),
                        path_str: path_str.clone(),
                        chunks: extracted.chunks,
                        chunk_hashes,
                        edges: extracted.edges,
                    };
                    match batcher.add_file(cache_hits, miss_texts, meta) {
                        AddOutcome::Complete(c) => {
                            warn_embed_issues(
                                handle,
                                &c.meta.path_str,
                                c.dim_mismatch,
                                c.dim_sample,
                                c.raw_failures,
                                c.miss_count,
                                state.config.embedding.dim,
                            );
                            let PendingFile {
                                entry,
                                path_str,
                                chunks,
                                chunk_hashes,
                                edges,
                            } = c.meta;
                            let (e, cw, h) = persist_completed_file(
                                state,
                                handle,
                                entry,
                                &path_str,
                                &chunks,
                                chunk_hashes,
                                &edges,
                                c.embeddings,
                                Some(embed_model.as_str()),
                                false,
                            )
                            .await;
                            entries_written += e;
                            chunks_written += cw;
                            hard_errors += h;
                            record_file_progress(
                                handle,
                                &path_str,
                                &mut done,
                                &mut samples,
                                n_files,
                            );
                        }
                        // Buffered: NOT persisted yet — no progress update here. Deferred to
                        // `flush_deep_batcher`'s per-file loop, which calls
                        // `record_file_progress` only once this file's data is actually
                        // written (see that function's doc comment and finding #6's fix).
                        AddOutcome::Buffered => {}
                    }
                    if batcher.is_full() {
                        let (e, cw, h) = flush_deep_batcher(
                            &mut batcher,
                            state,
                            handle,
                            &mut wdog,
                            &spec,
                            headroom,
                            ctx_llm.as_ref(),
                            &embed_model,
                            &mut done,
                            &mut samples,
                            n_files,
                        )
                        .await;
                        entries_written += e;
                        chunks_written += cw;
                        hard_errors += h;
                    }
                }
            } else {
                // No chunks were extracted for this file at all (e.g. binary/empty) — nothing
                // to persist, so this is already "done" the moment we know that.
                record_file_progress(handle, &path_str, &mut done, &mut samples, n_files);
            }
        }
    }

    // End-of-run tail flush: drain whatever partial batch is still buffered (below
    // `is_full()`'s threshold) now that every file has been walked, before the M5 check below
    // inspects the final `chunks_written`/`entries_written`/`hard_errors` totals. Each flushed
    // file's progress is advanced inside `flush_deep_batcher` itself, at the point it's
    // actually persisted.
    if !batcher.is_empty() {
        let (e, c, h) = flush_deep_batcher(
            &mut batcher,
            state,
            handle,
            &mut wdog,
            &spec,
            headroom,
            ctx_llm.as_ref(),
            &embed_model,
            &mut done,
            &mut samples,
            n_files,
        )
        .await;
        entries_written += e;
        chunks_written += c;
        hard_errors += h;
    }

    // M5: if there were files to process but nothing was written and nothing was
    // already current, and at least one file hard-errored, the phase genuinely
    // failed — don't let the caller report "complete". (A folder of binary/empty
    // files that simply yields no chunks is NOT a failure and still returns true.)
    // `summaries-only` never writes chunks by design, so `chunks_written` alone would
    // misfire here on any hard error even when most files succeeded — `entries_written`
    // is that mode's real success signal instead.
    let nothing_written = if skip_embed_work {
        entries_written == 0
    } else {
        chunks_written == 0
    };
    if !files.is_empty() && nothing_written && skipped == 0 && hard_errors > 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::config::Config;
    use indexa_core::store::Store;
    use indexa_embed::Embedder;

    /// Minimal `AppState` for driving `run_deep_phase` directly in a test — same shape as
    /// `crate::tests::state_with_embedder` (lib.rs), duplicated locally since that helper lives
    /// in a private `mod tests` this module can't reach.
    fn build_state(
        store: Store,
        machine_spec: MachineSpec,
        embedder: Arc<dyn Embedder + Send + Sync + 'static>,
    ) -> AppState {
        struct StubGenerator;
        #[async_trait::async_trait]
        impl indexa_llm::Generator for StubGenerator {
            async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
                Ok("stub".to_owned())
            }
        }
        static TAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = TAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut config_path = std::env::temp_dir();
        config_path.push(format!(
            "indexa-deep-test-config-{}-{tag}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&config_path);
        let (_tx, telemetry) = tokio::sync::watch::channel(crate::dto::TelemetrySample::default());
        let mut config = Config::default();
        // A near-zero headroom (rather than the multi-GB profile default) makes
        // `compute_budget`'s Ok/Throttle/Critical verdict depend only on `MachineSpec`
        // (specifically `gpu_wired_limit_bytes`, which each test controls directly) instead of
        // on how much RAM happens to be free on whatever machine runs the test — a multi-GB
        // default headroom would risk spuriously entering the (real-time, up to 300s) pause
        // loop on a memory-constrained CI runner.
        config.resource.headroom_gb = 0.001;
        AppState {
            store: Arc::new(tokio::sync::Mutex::new(store)),
            embedder,
            llm: Arc::new(StubGenerator),
            config: Arc::new(config),
            jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            db_path: Arc::new(std::path::PathBuf::from(":memory:")),
            config_path: Arc::new(config_path),
            log_dir: Arc::new(std::env::temp_dir()),
            walk_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
            machine_spec: Arc::new(machine_spec),
            telemetry,
            ann: Arc::new(tokio::sync::RwLock::new(crate::AnnCache::default())),
            ann_build_lock: Arc::new(tokio::sync::Mutex::new(())),
            watch_sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    fn write_test_file(
        dir: &std::path::Path,
        name: &str,
        marker: &str,
    ) -> indexa_core::walker::Entry {
        let path = dir.join(name);
        // Long enough, distinct-enough content that the default text parser/chunker (800-word
        // target chunks) always yields at least one real chunk — this is exercising the deep
        // loop's batching/progress/watchdog plumbing, not chunking edge cases.
        let body = format!(
            "{marker} — {}",
            "the quick brown fox jumps over the lazy dog. ".repeat(40)
        );
        std::fs::write(&path, &body).unwrap();
        let size = body.len() as u64;
        indexa_core::walker::Entry {
            path,
            kind: EntryKind::File,
            size,
            modified: Some(std::time::SystemTime::now()),
            hint: None,
            is_binary: false,
        }
    }

    fn default_machine_spec() -> MachineSpec {
        MachineSpec {
            total_ram_bytes: 8 * 1024 * 1024 * 1024,
            physical_cores: 4,
            logical_cores: 8,
            is_apple_silicon: false,
            gpu_wired_limit_bytes: 8 * 1024 * 1024 * 1024,
        }
    }

    /// Every call marks a shared, ordered log with "EMBED" before delegating to a fixed-dim stub
    /// embedding — the flush's `embed_all` round-trip is the one thing that can only happen
    /// after a batcher-buffered file's misses are resolved, so its position in the log relative
    /// to `Progress` events pins down whether progress fired before or after persistence.
    struct MarkingEmbedder {
        handle: Arc<JobHandle>,
    }
    #[async_trait::async_trait]
    impl Embedder for MarkingEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 8])
        }
        async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            push(
                &self.handle,
                JobEvent::Warning {
                    stage: "test-marker".to_owned(),
                    item_path: None,
                    message: "EMBED_CALLED".to_owned(),
                    pressure: None,
                },
            );
            Ok(vec![vec![0.0; 8]; texts.len()])
        }
        fn dim(&self) -> usize {
            8
        }
    }

    #[tokio::test]
    async fn deep_phase_never_reports_progress_before_the_file_is_persisted() {
        // Regression for finding #6: batching cross-file misses (#367/MissBatcher) must not
        // move the `Progress` event earlier than the point a file's data is actually written.
        // With ≥1 file and every chunk a fresh cache-miss (first-time indexing) below
        // `EMBED_BATCH_SIZE`, every file lands in `AddOutcome::Buffered` and is only persisted
        // at the end-of-run tail flush — so under the bug, ALL of these files' `Progress`
        // events would appear in `handle.history` BEFORE the single `EMBED_CALLED` marker
        // (pushed when the flush's `embed_all` finally runs). Fixed, none should.
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            write_test_file(dir.path(), "a.txt", "file-a"),
            write_test_file(dir.path(), "b.txt", "file-b"),
            write_test_file(dir.path(), "c.txt", "file-c"),
        ];

        let path_str = dir.path().to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        let handle = Arc::new(JobHandle::new("deep", path_str.clone()));
        let state = build_state(
            store,
            default_machine_spec(),
            Arc::new(MarkingEmbedder {
                handle: handle.clone(),
            }),
        );

        // `Progress` events are diverted to `handle.last_progress` (only the latest survives)
        // rather than appended to `handle.history` — see `jobs::push`'s doc comment — so the
        // ordered timeline this test needs has to come from the broadcast channel instead,
        // subscribed BEFORE the run so every event (including our EMBED_CALLED marker) is
        // captured in true send order.
        let mut rx = handle.tx.subscribe();

        let ok = run_deep_phase(&state, &path_str, &entries, &handle).await;
        assert!(ok, "deep phase should succeed against fresh temp files");

        let mut timeline = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            timeline.push(ev);
        }
        let first_embed_idx = timeline
            .iter()
            .position(
                |e| matches!(e, JobEvent::Warning { message, .. } if message == "EMBED_CALLED"),
            )
            .expect("embed_batch must have been called at least once");
        let premature_progress = timeline[..first_embed_idx]
            .iter()
            .filter(|e| matches!(e, JobEvent::Progress { .. }))
            .count();
        assert_eq!(
            premature_progress, 0,
            "no Progress event should fire before the batcher's embed round-trip persists \
             anything — got {premature_progress} premature Progress event(s) in {timeline:#?}"
        );
        let total_progress = timeline
            .iter()
            .filter(|e| matches!(e, JobEvent::Progress { .. }))
            .count();
        assert_eq!(
            total_progress,
            entries.len(),
            "exactly one Progress event per file expected"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deep_phase_checks_the_watchdog_per_file_not_only_at_batch_flush() {
        // Regression for finding #2: batching cross-file misses (#367/MissBatcher) must not
        // widen the watchdog's pause cadence from "before every file's embed work" to "before
        // every ~EMBED_BATCH_SIZE-file flush". `gpu_wired_limit_bytes: 0` deterministically
        // forces every pressure sample to Critical (`compute_budget` clamps truly-available RAM
        // to 0 regardless of the real host), independent of the actual test machine's memory —
        // so every `run_watchdog_check` call pushes exactly one "Low on memory" entry Warning
        // before looping into its (here, virtual-time — `start_paused = true`) recovery wait.
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            write_test_file(dir.path(), "a.txt", "file-a"),
            write_test_file(dir.path(), "b.txt", "file-b"),
        ];

        let path_str = dir.path().to_string_lossy().into_owned();
        let store = Store::open_in_memory().unwrap();
        let handle = Arc::new(JobHandle::new("deep", path_str.clone()));
        let mut spec = default_machine_spec();
        spec.gpu_wired_limit_bytes = 0;
        let state = build_state(
            store,
            spec,
            Arc::new(MarkingEmbedder {
                handle: handle.clone(),
            }),
        );

        run_deep_phase(&state, &path_str, &entries, &handle).await;

        let history = handle.history.lock().unwrap().clone();
        let watchdog_checks = history
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    JobEvent::Warning { message, .. }
                        if message.contains("Easing off and freeing the model")
                )
            })
            .count();
        // 2 files (per-file, before each registers its misses) + 1 end-of-run tail flush.
        // Before the fix this would be 1 (the tail flush only) regardless of file count.
        assert_eq!(
            watchdog_checks,
            entries.len() + 1,
            "watchdog must be checked once per file plus once per flush, not only per flush: \
             got {watchdog_checks} checks for {} files",
            entries.len()
        );
    }
}
