use anyhow::Result;
use indexa_core::{
    config::{Config, SummaryMode},
    resource::{
        assess, detect_machine, estimate_eta, format_duration_pub, pause_step, MachineSpec,
        PauseAction, Pressure, WatchdogState, MAX_PAUSE_SECS,
    },
    store::{chunk_content_hash, ChunkRecord, EdgeRecord, Store, SymbolRecord},
    walker::{walk, WalkConfig},
};
use indexa_embed::{AddOutcome, MissBatcher};
use indexa_llm::OllamaLlm;
use indexa_query::{contextual::ContextualEvent, enqueue_subtree, redact::chunk_text_for_store};
use std::io::{IsTerminal, Write};

use super::helpers::{
    build_embedder, preflight_ollama, require_index_db, resolve_summary_mode, resolve_target_roots,
};

/// Per-file payload buffered in the cross-file [`MissBatcher`] between registration and the
/// flush that resolves its cache-miss embeddings — everything [`persist_completed_file`] needs
/// that isn't already captured in the resolved `embeddings` vector. Closes #367: previously
/// every file with ≥1 cache-miss chunk issued its own `embed_all` round-trip; batching these
/// across files cuts a deep index's HTTP round-trips roughly `EMBED_BATCH_SIZE`-fold, without
/// changing what gets stored (see `crates/embed/src/batcher.rs`'s correctness note).
struct PendingFile {
    entry: indexa_core::walker::Entry,
    path_str: String,
    chunks: Vec<indexa_parsers::types::Chunk>,
    chunk_hashes: Vec<String>,
    edges: Vec<indexa_parsers::types::Edge>,
}

/// Print the same dim-mismatch / embed-failure warnings the old per-file path printed, now
/// driven off a `Completed`'s aggregated counts. `raw_failures` and `dim_mismatch` are tracked
/// separately by `MissBatcher::scatter`; the old code's `embed_failures` count (computed AFTER
/// `enforce_embedding_dim` nulled mismatched slots) is exactly their sum, so summing here
/// reproduces the same printed numbers.
fn warn_embed_issues(
    path_str: &str,
    dim_mismatch: usize,
    dim_sample: Option<usize>,
    raw_failures: usize,
    miss_count: usize,
    configured_dim: usize,
) {
    if dim_mismatch > 0 {
        eprintln!(
            "  ⚠  {dim_mismatch} chunk(s) in {path_str} embedded at dim {} ≠ configured {} \
             — stored text-only; fix [embedding] model/dim and re-run deep.",
            dim_sample.unwrap_or(0),
            configured_dim
        );
    }
    let embed_failures = raw_failures + dim_mismatch;
    if embed_failures > 0 && dim_mismatch == 0 {
        eprintln!(
            "  ⚠  {embed_failures}/{miss_count} chunk(s) in {path_str} failed to embed (stored text-only)."
        );
    }
}

/// Memory-pressure watchdog for the CLI `deep` command. `indexa deep` never had one at all
/// (confirmed via git history predating even the cross-file `MissBatcher` change) — a
/// separate, pre-existing gap from the one #505 fixed on the web-job side
/// (`crates/web/src/jobs_exec/watchdog.rs`'s `run_watchdog_check`). Mirrors that function's
/// logic — same `indexa_core::resource` primitives, same recover-aware entry gate, same
/// unload-once-on-Critical + capped recovery-wait shape as `crates/query/src/worker.rs`'s
/// `run_worker` — but driven by `eprintln!` (matching this file's own warning convention, see
/// `warn_embed_issues`) instead of a `JobEvent::Warning` push, since `JobHandle`/`JobEvent` are
/// web-only constructs that must not be pulled into `apps/indexa` (wrong dependency direction).
///
/// Called (1) per-file, right before a file registers its cache misses with the cross-file
/// `MissBatcher`, and (2) inside `flush_deep_batcher`, before its `embed_all` round-trip —
/// batching cross-file misses (#367) must not widen the pause cadence to "once per flush".
async fn check_deep_watchdog(
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    embedder: Option<&(dyn indexa_embed::Embedder + Send + Sync)>,
    ctx_llm: Option<&(dyn indexa_llm::Describer + Send + Sync)>,
) {
    let sample = wdog.sample();
    // Gate entry on the same recover-aware predicate as resume, not raw `assess()` — macOS
    // swap is sticky, so `assess()` keeps reporting Critical for the rest of the run even after
    // RAM has actually recovered, which would re-pause (and reload the model) on every
    // subsequent file. `pause_step(.., 0) == Resume` means "RAM is fine OR no real signal".
    if pause_step(spec, &sample, headroom, 0) == PauseAction::Resume {
        return;
    }
    // RAM is genuinely low. Use `assess()` only to choose the unload gate (Critical vs Throttle).
    let pressure = assess(&sample, spec, headroom);
    let avail_gb = sample.available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let swap_gb = sample.swap_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    eprintln!(
        "  ⚠  low on memory (available: {avail_gb:.1} GB, swap: {swap_gb:.1} GB) — easing off \
         and freeing the model; this resumes automatically."
    );

    // On a Critical entry, unload the resident model(s) once so their wired pages free and
    // `compute_budget` can climb back above 0 — macOS swap is sticky and never drains on its
    // own, so gating resume on swap level alone would stall here for the full backstop.
    if pressure == Pressure::Critical {
        if let Some(e) = embedder {
            e.unload().await;
        }
        if let Some(l) = ctx_llm {
            l.unload().await;
        }
    }

    // Wait until memory actually recovers, capped at `MAX_PAUSE_SECS`. `pause_step` re-evaluates
    // a fresh sample each tick: it resumes when free RAM returns above headroom (recovery)
    // regardless of sticky swap, and escalation (Throttle → Critical) tightens the cadence
    // immediately.
    let mut elapsed = 0u64;
    loop {
        match pause_step(spec, &wdog.sample(), headroom, elapsed) {
            PauseAction::Resume => break,
            PauseAction::Proceed => {
                eprintln!(
                    "  ⚠  memory didn't recover within {MAX_PAUSE_SECS}s — continuing gently."
                );
                break;
            }
            PauseAction::Sleep(secs) => {
                tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
                elapsed += secs;
            }
        }
    }
}

/// Build chunk records from a fully-resolved `embeddings` vector and persist one file: entries,
/// then chunks (unless `summaries_only`), then best-effort edges/symbols. Shared by the
/// `--no-embed`/summaries-only path (embeddings all `None`, resolved synchronously), a
/// zero-miss `MissBatcher::add_file` completion, and a post-`scatter` completion — all three
/// converge here once a file's embeddings are known. Returns the number of chunk records
/// written (0 when `summaries_only`), for the caller's running `total_chunks`.
#[allow(clippy::too_many_arguments)] // one flat finalize; grouping would just move fields around
fn persist_completed_file(
    store: &mut Store,
    entry: indexa_core::walker::Entry,
    path_str: &str,
    chunks: &[indexa_parsers::types::Chunk],
    chunk_hashes: Vec<String>,
    edges: &[indexa_parsers::types::Edge],
    embeddings: Vec<Option<Vec<f32>>>,
    embed_model: Option<&str>,
    summaries_only: bool,
    redact_at_index: bool,
) -> Result<usize> {
    let mut chunk_records = Vec::with_capacity(chunks.len());
    for ((chunk, embedding), hash) in chunks.iter().zip(embeddings).zip(chunk_hashes) {
        // Redact obvious secrets before writing to the searchable store (shared choke point so
        // every index path — deep + watch, CLI + web — behaves identically).
        let text = chunk_text_for_store(&chunk.text, redact_at_index);
        chunk_records.push(ChunkRecord {
            entry_path: path_str.to_owned(),
            seq: chunk.seq,
            heading: chunk.heading.clone(),
            text,
            language: chunk.language.clone(),
            embedding,
            embed_model: embed_model.map(|m| m.to_owned()),
            content_hash: Some(hash),
        });
    }

    // `deep` can run without a preceding `scan` (see the caller's original comment), so without
    // this the file has no `entries` row — its chunks are orphans: never summarized and silently
    // deleted the next time `prune_orphans` runs. Always written regardless of mode —
    // `summaries-only` still needs a live entries row so the file is summarizable.
    store.upsert_entries(&[entry])?;
    // `summaries-only` never persists chunk rows — that's the entire ~100× size win;
    // `summarize_file` re-parses the file itself when no chunks are stored.
    let written = if summaries_only {
        0
    } else {
        store.upsert_chunks(&chunk_records)?;
        chunk_records.len()
    };

    // Persist the file's code-graph edges (imports/defines) keyed on the same entry-path
    // string as its chunks, so `edges_from(path)` lines up with search. Best-effort: a
    // failure warns rather than aborting the scan.
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
            eprintln!(
                "  ⚠  {path_str}: failed to store {} code-graph edge(s): {e:#}",
                edge_records.len()
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
                eprintln!(
                    "  ⚠  {path_str}: failed to store {} symbol(s): {e:#}",
                    symbol_records.len()
                );
            }
        }
    }
    Ok(written)
}

/// Flush the batcher: embed every currently-buffered cross-file miss in one (internally
/// sub-batched) round-trip, scatter results back to owning files, and persist every file that
/// completes. Called at `is_full()` and once more at end-of-root to drain a final partial batch.
///
/// Checks the memory watchdog once, right before `embed_all`, mirroring the per-file check the
/// caller runs before ever registering a file's misses here (see `check_deep_watchdog`'s doc
/// comment) — this is the "once per flush" half of that two-point cadence.
#[allow(clippy::too_many_arguments)] // mirrors persist_completed_file's existing precedent for this same allow
async fn flush_deep_batcher(
    batcher: &mut MissBatcher<PendingFile>,
    embedder: &(dyn indexa_embed::Embedder + Send + Sync),
    store: &mut Store,
    embed_model: &str,
    configured_dim: usize,
    redact_at_index: bool,
    wdog: &mut WatchdogState,
    spec: &MachineSpec,
    headroom: u64,
    ctx_llm: Option<&(dyn indexa_llm::Describer + Send + Sync)>,
) -> Result<usize> {
    check_deep_watchdog(wdog, spec, headroom, Some(embedder), ctx_llm).await;
    let refs = batcher.batch_refs();
    let results = indexa_embed::embed_all(embedder, &refs, indexa_embed::EMBED_BATCH_SIZE).await;
    drop(refs);
    let mut written = 0usize;
    for c in batcher.scatter(results) {
        warn_embed_issues(
            &c.meta.path_str,
            c.dim_mismatch,
            c.dim_sample,
            c.raw_failures,
            c.miss_count,
            configured_dim,
        );
        let PendingFile {
            entry,
            path_str,
            chunks,
            chunk_hashes,
            edges,
        } = c.meta;
        // Never `summaries_only` here — `skip_embed_work` (which includes summaries-only)
        // bypasses the batcher entirely and finalizes inline instead (see `cmd_deep`).
        written += persist_completed_file(
            store,
            entry,
            &path_str,
            &chunks,
            chunk_hashes,
            &edges,
            c.embeddings,
            Some(embed_model),
            false,
            redact_at_index,
        )?;
    }
    Ok(written)
}

#[allow(clippy::too_many_arguments)] // thin CLI fan-out; grouping into a struct would just move fields
pub(crate) async fn cmd_deep(
    paths: Vec<String>,
    embed_model_flag: Option<String>,
    dry_run: bool,
    mode: Option<String>,
    contextual: bool,
    contextual_prefix: bool,
    no_embed: bool,
    cfg: &Config,
) -> Result<()> {
    // ── Preflight: confirm Ollama is up and required models are pulled ─────────
    // Skip during dry-run (no actual model calls are made) and in `--no-embed`
    // mode (FTS-only: no embeddings, no contextual/caption/transcribe/OCR calls,
    // so no model needs to be reachable — this is what makes a CI run hermetic).
    // NOT skipped for `summaries-only` alone: that mode still needs the file_model/
    // dir_model describer check preflighted here, since `cmd_index`'s later summarize
    // phase is the one that actually calls them.
    if !dry_run && !no_embed {
        preflight_ollama(cfg).await?;
    }

    let summary_mode = resolve_summary_mode(mode.as_deref(), cfg.describer.mode.clone())?;
    // `summaries-only` never stores chunks, so every model call that only exists to enrich
    // stored chunks (embeddings, contextual blurbs, image captions, audio transcription, PDF
    // OCR) is wasted work here — treat it exactly like `--no-embed` for all of those, while
    // still parsing + writing entries/edges/symbols. `summarize_file` re-parses the file
    // itself when no chunks are stored (`sample_via_parse` in `indexa_query::summarize`), so
    // this phase doesn't need to feed it a sample — but that means the describer prompt is
    // built from the default 800/100-word registry, not this pass's `[chunking]`/parser config.
    let skip_embed_work = no_embed || summary_mode == SummaryMode::SummariesOnly;
    let roots = resolve_target_roots(paths, false)?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let max_parse_bytes = cfg.parsers.max_file_mb.saturating_mul(1024 * 1024);
    // Parser registry honoring `[chunking]` size/overlap; built once and reused for every file
    // (both the dry-run count and the real parse) instead of the free `parse_guarded`.
    let registry = super::helpers::chunk_registry(cfg);
    let walk_cfg = WalkConfig {
        respect_gitignore: cfg.scan.respect_gitignore,
        ignore: cfg.scan.ignore.clone(),
        // Whole-computer groundwork: when `[scan] skip_binary` is on, the walk NUL-sniffs files
        // so the loops below can skip binaries without opening them for a parse attempt.
        sniff_binary: cfg.scan.skip_binary,
        threads: cfg.scan.threads,
        custom_ignore: cfg.scan.custom_ignore,
        ..Default::default()
    };

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();

    if dry_run {
        println!("Dry run — nothing will be written to the index.\n");
        // Collect the file set (path + size) across roots up front; family is classified by
        // extension (cheap, no parse) so it's always exact and independent of the chunk-count
        // sampling below — deliberately a coarser, faster axis than the old MIME-sniffed
        // breakdown (see `classify_file_by_extension`'s `category` — extension-based
        // classification misses files with wrong/missing extensions that MIME sniffing would
        // have caught, e.g. an extensionless script or a mislabeled `.txt`).
        let mut all_files: Vec<(std::path::PathBuf, u64)> = Vec::new();
        let mut by_family: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for root in &roots {
            let entries = walk(root, &walk_cfg)?;
            for e in entries
                .iter()
                .filter(|e| e.kind == indexa_core::walker::EntryKind::File && !e.is_binary)
            {
                let family = indexa_core::surface::classify_file_by_extension(&e.path)
                    .map(|h| h.category.to_owned())
                    .unwrap_or_else(|| "other".to_owned());
                *by_family.entry(family).or_default() += 1;
                all_files.push((e.path.clone(), e.size));
            }
        }
        let total_files = all_files.len();
        // `walk()` honors `[scan] threads` and returns entries in whatever order the parallel
        // walk completed in — nondeterministic across runs. Sort before sampling so the
        // evenly-spaced sample (and therefore the estimate below) is a deterministic function of
        // the file set: an unchanged tree reports the same estimated chunk count every time,
        // rather than a different sample producing a different number on each `--dry-run`.
        all_files.sort();

        // Chunk count: a tree at or below `DRY_RUN_SAMPLE_MAX` files is parsed in full (the
        // sample IS the whole set, so the count is exact). A larger tree parses an
        // evenly-spaced sample of that many files and extrapolates chunks-per-byte to the
        // whole tree — trading a few percent of accuracy for a preview that doesn't take
        // nearly as long as the real deep run it's previewing. See `estimate_total_chunks`.
        const DRY_RUN_SAMPLE_MAX: usize = 64;
        let (total_chunks, sampled, was_sampled) =
            estimate_total_chunks(&all_files, DRY_RUN_SAMPLE_MAX, max_parse_bytes, |p, sz| {
                registry
                    .parse_guarded(p, sz, max_parse_bytes)
                    .ok()
                    .map(|ex| ex.chunks.len())
            });

        if was_sampled {
            println!(
                "Would parse {total_files} files (chunks estimated from a {sampled}-file sample):"
            );
        } else {
            println!("Would parse {total_files} files:");
        }
        let mut pairs: Vec<_> = by_family.into_iter().collect();
        pairs.sort_by_key(|b| std::cmp::Reverse(b.1));
        for (family, n) in pairs {
            println!("  {:>5}  {family}", n);
        }
        println!("\nEstimated embedding calls: {total_chunks} chunks");
        // Use the calibrated ETA table instead of the old hardcoded 300 chunks/min.
        let spec = detect_machine();
        let embed_eta = estimate_eta(&embed_model, 0, total_chunks, 0, 1, spec.is_apple_silicon);
        let sum_eta = estimate_eta(
            &cfg.describer.file_model,
            total_files,
            0,
            600,
            cfg.describer.passes_first,
            spec.is_apple_silicon,
        );
        println!(
            "Estimated time: {} embed + {} summarize = {} total",
            embed_eta.display,
            sum_eta.display,
            format_duration_pub((embed_eta.total_secs + sum_eta.total_secs) as u64),
        );
        println!(
            "  (model: {embed_model} + {}, Apple Silicon: {})",
            cfg.describer.file_model, spec.is_apple_silicon
        );
        println!("  Run `indexa doctor --files {total_files} --chunks {total_chunks}` for a full breakdown.");
        return Ok(());
    }

    let mut store = Store::open(&db_path)?;
    // `--no-embed` / `summaries-only` build no embedder at all.
    let embedder = if skip_embed_work {
        None
    } else {
        Some(build_embedder(cfg, Some(&embed_model))?)
    };

    // Memory-pressure watchdog: `indexa deep` never had one at all (see `check_deep_watchdog`'s
    // doc comment). Detected once and reused across every root/file this invocation walks —
    // cheap, and `WatchdogState`'s `sysinfo::System` is meant to be long-lived per job.
    let watchdog_spec = detect_machine();
    let watchdog_headroom = cfg.resource.effective_headroom_bytes();
    let mut wdog = WatchdogState::new();

    // Effective contextual-retrieval flag: CLI --contextual OR config [describer] contextual_retrieval.
    // Forced off when embedding work is skipped — a situating blurb needs an LLM call.
    let use_contextual = !skip_embed_work && (contextual || cfg.describer.contextual_retrieval);
    // Effective deterministic contextual-prefix flag: CLI --contextual-prefix OR config
    // [describer] contextual_prefix. Forced off when embedding work is skipped (nothing is embedded).
    let use_prefix = !skip_embed_work && (contextual_prefix || cfg.describer.contextual_prefix);
    // Build the contextual LLM once (re-used per file) when the feature is enabled.
    // Uses the same file-describer model and base URL — no extra model pull needed.
    let ctx_llm: Option<OllamaLlm> = if use_contextual {
        let base = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
        Some(OllamaLlm::new(&base, &cfg.describer.file_model).with_num_ctx(cfg.describer.num_ctx))
    } else {
        None
    };
    if use_contextual {
        eprintln!(
            "  contextual retrieval enabled (model: {})",
            cfg.describer.file_model
        );
    }

    // Optional image captioning (opt-in): a vision model adds a caption chunk per image.
    // Built once, gated on [parsers.image] caption; shares the describer's Ollama endpoint.
    let captioner = if !skip_embed_work && cfg.parsers.image.caption {
        let base = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
        Some(
            OllamaLlm::new(&base, cfg.parsers.image.caption_model())
                .with_num_ctx(cfg.describer.num_ctx),
        )
    } else {
        None
    };
    let caption_model = cfg.parsers.image.caption_model().to_owned();
    // Optional audio transcription (opt-in): a whisper.cpp-style CLI per audio file.
    // Disabled when embedding work is skipped (that path makes no model/tool calls).
    let transcribe = !skip_embed_work && cfg.parsers.audio.transcribe;
    let transcribe_binary = cfg.parsers.audio.transcribe_binary().to_owned();
    let transcribe_model = cfg.parsers.audio.model.clone();
    // Optional PDF OCR (opt-in): pdftoppm + tesseract for scanned PDFs with no text layer.
    // Disabled when embedding work is skipped (that path makes no model/tool calls).
    let ocr_enabled = !skip_embed_work && cfg.parsers.pdf.ocr_enabled();
    let ocr_binary = cfg.parsers.pdf.ocr_binary().to_owned();
    let ocr_lang = cfg.parsers.pdf.ocr_lang.clone();

    for root in &roots {
        if no_embed {
            println!(
                "Deep-scanning {} (FTS only — no embeddings)",
                root.display()
            );
        } else if summary_mode == SummaryMode::SummariesOnly {
            println!(
                "Deep-scanning {} (summaries-only — chunks not stored)",
                root.display()
            );
        } else {
            println!(
                "Deep-scanning {} with embed model '{}'",
                root.display(),
                embed_model
            );
        }
        let entries = walk(root, &walk_cfg)?;
        // `[scan] skip_binary` sniffs binaries during the walk; skip them here so `deep`/`index`
        // never opens/parses an executable/image/DB blob (matches the dry-run + web deep paths).
        // Secret files (`.env`, keys, `.pem`/keystores) are recorded by scan but not embedded
        // unless `[scan] include_sensitive` — redaction can't scrub a raw key, so their contents
        // stay out of the searchable index by default.
        let include_sensitive = walk_cfg.include_sensitive;
        let is_sensitive = |e: &&indexa_core::walker::Entry| {
            e.hint
                .as_ref()
                .is_some_and(|h| h.deep_scan == indexa_core::surface::DeepScanPolicy::Sensitive)
        };
        let files: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.kind == indexa_core::walker::EntryKind::File
                    && !e.is_binary
                    && (include_sensitive || !is_sensitive(e))
            })
            .collect();
        let binaries_skipped = entries
            .iter()
            .filter(|e| e.kind == indexa_core::walker::EntryKind::File && e.is_binary)
            .count();
        let sensitive_skipped = if include_sensitive {
            0
        } else {
            entries
                .iter()
                .filter(|e| {
                    e.kind == indexa_core::walker::EntryKind::File
                        && !e.is_binary
                        && is_sensitive(e)
                })
                .count()
        };

        let mut notes: Vec<String> = Vec::new();
        if binaries_skipped > 0 {
            notes.push(format!("{binaries_skipped} binaries"));
        }
        if sensitive_skipped > 0 {
            notes.push(format!("{sensitive_skipped} sensitive"));
        }
        if notes.is_empty() {
            println!("  parsing {} files...", files.len());
        } else {
            println!(
                "  parsing {} files ({} skipped)...",
                files.len(),
                notes.join(", ")
            );
        }
        let mut total_chunks = 0usize;
        let mut skipped = 0usize;
        // Accumulates cache-miss embed-texts across this root's files so `embed_all` runs on
        // full `EMBED_BATCH_SIZE` batches instead of one file's 1–3 misses at a time (#367).
        // Scoped per root (not across `roots`) to keep this loop's existing per-root reporting
        // (`total_chunks` etc.) exact — flushed at `is_full()` below and once more at this
        // root's end-of-loop tail flush. Unused (never receives `add_file`) whenever
        // `skip_embed_work` is set, since that path finalizes inline without embedding at all.
        let mut batcher: MissBatcher<PendingFile> =
            MissBatcher::new(cfg.embedding.dim, indexa_embed::EMBED_BATCH_SIZE);

        // Lightweight in-place progress on stderr (carriage-return rewrite), shown only when
        // stderr is a terminal so piped/CI output stays clean. Hand-rolled to avoid pulling in
        // indicatif, whose transitive `number_prefix` dep is flagged unmaintained (RUSTSEC-2025-0119).
        let show_progress = std::io::stderr().is_terminal();
        let total_files = files.len();
        let prog_start = std::time::Instant::now();

        for (i, entry) in files.iter().enumerate() {
            if show_progress {
                let name = entry
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                // Cumulative rate + ETA so a long deep pass shows it's progressing, not frozen.
                let done = i + 1;
                let elapsed = prog_start.elapsed().as_secs_f64();
                let rate = if elapsed > 0.5 {
                    done as f64 / elapsed
                } else {
                    0.0
                };
                let eta = if rate > 0.0 {
                    indexa_core::resource::format_duration_pub(
                        ((total_files - done) as f64 / rate) as u64,
                    )
                } else {
                    "—".to_string()
                };
                eprint!("\r\x1b[K  [{done}/{total_files}] {rate:.0}/s · ETA {eta} · {name:.40}");
                let _ = std::io::stderr().flush();
            }
            let path_str = entry.path.to_string_lossy().into_owned();

            // Skip-if-unchanged: re-embedding is expensive; skip files whose chunks
            // are already indexed at or after the file's last modification time.
            // Compare against the *fresh* on-disk mtime from this walk, not the DB's
            // `modified_s` — `deep` can run without a preceding `scan`, so the stored
            // mtime may be stale and would wrongly skip an edited file (the web
            // pipeline avoids this by re-scanning first). Fall back to the stored
            // check when the filesystem gives us no mtime.
            // NOTE: in `summaries-only` mode this check can never see a chunk row (none are
            // ever stored), so it always re-parses every file on every pass — cheap relative
            // to embedding, but the dominant cost once nothing is embedded. Known, not a bug;
            // a real fix needs a freshness marker independent of the chunks table.
            let mtime_secs = entry
                .modified
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let is_current = match mtime_secs {
                Some(m) => store
                    .chunks_current_for_mtime(&path_str, m)
                    .unwrap_or(false),
                None => store.chunks_are_current(&path_str).unwrap_or(false),
            };
            if is_current {
                skipped += 1;
                continue;
            }

            let mut extracted =
                match registry.parse_guarded(&entry.path, entry.size, max_parse_bytes) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

            // Image captioning (opt-in): append a vision-model caption as an extra chunk
            // (kept alongside the EXIF chunk, not replacing it — both are searchable).
            if let Some(cap) = &captioner {
                if extracted.mime.starts_with("image/") {
                    match indexa_llm::caption_image_file(cap, &caption_model, &entry.path).await {
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
                        Err(e) => {
                            // Warn unconditionally (clearing the progress line first on a TTY)
                            // so the failure isn't lost on piped/CI runs.
                            if show_progress {
                                eprint!("\r\x1b[K");
                            }
                            eprintln!("  caption failed for {path_str}: {e:#}");
                        }
                    }
                }
            }

            // Audio transcription (opt-in): append a whisper transcript as an extra chunk
            // alongside the ffprobe metadata chunk. Blocking subprocess → spawn_blocking.
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
                    Ok(Err(e)) => {
                        if show_progress {
                            eprint!("\r\x1b[K");
                        }
                        eprintln!("  transcription failed for {path_str}: {e:#}");
                    }
                    Err(e) => {
                        if show_progress {
                            eprint!("\r\x1b[K");
                        }
                        eprintln!("  transcription task panicked for {path_str}: {e}");
                    }
                }
            }

            // PDF OCR (opt-in): a scanned PDF with no text layer gets rasterised + OCR'd and
            // the recognised text appended as an extra chunk. Blocking subprocess →
            // spawn_blocking; fails open (a missing tool just leaves the text-layer stub).
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
                        Ok(Err(e)) => {
                            if show_progress {
                                eprint!("\r\x1b[K");
                            }
                            eprintln!("  OCR failed for {path_str}: {e:#}");
                        }
                        Err(e) => {
                            if show_progress {
                                eprint!("\r\x1b[K");
                            }
                            eprintln!("  OCR task panicked for {path_str}: {e}");
                        }
                    }
                }
            }

            if extracted.chunks.is_empty() {
                continue;
            }

            // Compute SHA-256 of each chunk's raw text — used as a cache key to skip
            // re-embedding chunks whose content is unchanged since the last deep run.
            // The hash is over the ORIGINAL text (not the enriched blurb) so the cache
            // stays valid across contextual-retrieval runs on the same source text.
            let chunk_hashes: Vec<String> = extracted
                .chunks
                .iter()
                .map(|c| chunk_content_hash(&c.text))
                .collect();

            // Resolve a per-chunk embedding vector (aligned to `extracted.chunks`).
            // `--no-embed` stores every chunk text-only (vector = None) for sparse/FTS
            // search; a later plain `deep` self-heals them (the skip-if-current check
            // requires COUNT(*) = COUNT(embedding), so vector-less chunks aren't "current").
            // `summaries-only` never stores the chunk at all, so its embedding is moot —
            // skip computing one exactly like `--no-embed` does. Neither mode ever touches the
            // cross-file batcher — both finalize this file synchronously, right here.
            if skip_embed_work {
                let all_embeddings: Vec<Option<Vec<f32>>> = vec![None; extracted.chunks.len()];
                total_chunks += persist_completed_file(
                    &mut store,
                    (**entry).clone(),
                    &path_str,
                    &extracted.chunks,
                    chunk_hashes,
                    &extracted.edges,
                    all_embeddings,
                    None,
                    summary_mode == SummaryMode::SummariesOnly,
                    cfg.scan.redact_at_index,
                )?;
            } else {
                // Load the cached embedding map for this file (hash → Vec<f32>).
                // Fail-open: if the lookup errors (e.g. first run, column missing), treat as empty.
                let hash_cache = store
                    .cached_embeddings_by_hash(&path_str)
                    .unwrap_or_default();

                // Partition chunks into cache-hits (no embed needed) and misses (must embed).
                // A hit requires BOTH a matching hash AND a stored non-NULL vector.
                let mut cache_hits: Vec<Option<Vec<f32>>> = vec![None; extracted.chunks.len()];
                let mut miss_indices: Vec<usize> = Vec::new();
                for (i, hash) in chunk_hashes.iter().enumerate() {
                    if let Some(cached_vec) = hash_cache.get(hash) {
                        cache_hits[i] = Some(cached_vec.clone());
                    } else {
                        miss_indices.push(i);
                    }
                }

                // Build embed-text only for cache-miss chunks. With contextual retrieval
                // enabled, enrich each miss chunk with a situating blurb before embedding.
                let miss_raw_texts: Vec<&str> = miss_indices
                    .iter()
                    .map(|&i| extracted.chunks[i].text.as_str())
                    .collect();
                let miss_embed_texts: Vec<String> = if !miss_raw_texts.is_empty() {
                    if let Some(ref llm) = ctx_llm {
                        // Build doc context from the FULL file (all chunks), not just misses,
                        // so the situating blurbs are grounded in the whole document.
                        let all_raw: Vec<&str> =
                            extracted.chunks.iter().map(|c| c.text.as_str()).collect();
                        let doc_context = indexa_query::contextual::build_doc_context(&all_raw);
                        let path_str_clone = path_str.clone();
                        indexa_query::contextual::contextual_embed_texts(
                            llm,
                            &doc_context,
                            &miss_raw_texts,
                            None,
                            &path_str,
                            move |event| match event {
                                ContextualEvent::BlurbFragment { .. } => {}
                                ContextualEvent::BlurbFailed { error, .. } => {
                                    eprintln!(
                                        "  ⚠  {path_str_clone}: context blurb failed: {error}"
                                    );
                                }
                            },
                        )
                        .await
                    } else if use_prefix {
                        // Deterministic, local, no-LLM contextual prefix: prepend the file path,
                        // section heading, and a document-context snippet to each miss chunk's
                        // embed input. Grounds the embedding in the whole document at zero token
                        // cost. Applied to embed text ONLY — the stored/hashed text is untouched.
                        let all_raw: Vec<&str> =
                            extracted.chunks.iter().map(|c| c.text.as_str()).collect();
                        let doc_context = indexa_query::contextual::build_doc_context(&all_raw);
                        let miss_headings: Vec<&str> = miss_indices
                            .iter()
                            .map(|&i| extracted.chunks[i].heading.as_str())
                            .collect();
                        indexa_query::contextual::contextual_prefix_texts(
                            &doc_context,
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
                // cross-file batcher, not just at flush points (`is_full()`/end-of-root) —
                // batching the embed round-trips (#367/MissBatcher) must not widen the pause
                // cadence: up to `EMBED_BATCH_SIZE` files' worth of parsed chunks/edges/
                // LLM-enrichment could otherwise accumulate before a Critical-pressure pause is
                // even checked. This restores per-file cadence; `flush_deep_batcher` checks
                // again right before the actual (and possibly much-later) `embed_all` round-trip.
                check_deep_watchdog(
                    &mut wdog,
                    &watchdog_spec,
                    watchdog_headroom,
                    embedder.as_deref(),
                    ctx_llm
                        .as_ref()
                        .map(|l| l as &(dyn indexa_llm::Describer + Send + Sync)),
                )
                .await;

                // Register this file's misses with the cross-file batcher instead of embedding
                // them here directly (#367) — the batcher accumulates buffered embed-texts
                // across files so the eventual `embed_all` call runs on a full batch rather
                // than this one file's lone 1–3 misses. `add_file` finalizes a zero-miss file
                // (`miss_indices` empty) synchronously and it never touches the buffer — this
                // is also why a cache-hit chunk never "enters the batcher": only miss
                // embed-texts are ever buffered; `cache_hits`' `Some` slots ride along in the
                // `embeddings` vector and are returned untouched.
                let miss_texts: Vec<(usize, String)> =
                    miss_indices.into_iter().zip(miss_embed_texts).collect();
                let meta = PendingFile {
                    entry: (**entry).clone(),
                    path_str,
                    chunks: extracted.chunks,
                    chunk_hashes,
                    edges: extracted.edges,
                };
                match batcher.add_file(cache_hits, miss_texts, meta) {
                    AddOutcome::Complete(c) => {
                        warn_embed_issues(
                            &c.meta.path_str,
                            c.dim_mismatch,
                            c.dim_sample,
                            c.raw_failures,
                            c.miss_count,
                            cfg.embedding.dim,
                        );
                        let PendingFile {
                            entry,
                            path_str,
                            chunks,
                            chunk_hashes,
                            edges,
                        } = c.meta;
                        total_chunks += persist_completed_file(
                            &mut store,
                            entry,
                            &path_str,
                            &chunks,
                            chunk_hashes,
                            &edges,
                            c.embeddings,
                            Some(embed_model.as_str()),
                            false,
                            cfg.scan.redact_at_index,
                        )?;
                    }
                    AddOutcome::Buffered => {}
                }
                if batcher.is_full() {
                    total_chunks += flush_deep_batcher(
                        &mut batcher,
                        embedder
                            .as_deref()
                            .expect("embedder is built whenever embedding work isn't skipped"),
                        &mut store,
                        &embed_model,
                        cfg.embedding.dim,
                        cfg.scan.redact_at_index,
                        &mut wdog,
                        &watchdog_spec,
                        watchdog_headroom,
                        ctx_llm
                            .as_ref()
                            .map(|l| l as &(dyn indexa_llm::Describer + Send + Sync)),
                    )
                    .await?;
                }
            }
        }

        // End-of-root tail flush: drain whatever partial batch is still buffered (below
        // `is_full()`'s threshold) now that every file in this root has been walked.
        if !batcher.is_empty() {
            total_chunks += flush_deep_batcher(
                &mut batcher,
                embedder
                    .as_deref()
                    .expect("embedder is built whenever embedding work isn't skipped"),
                &mut store,
                &embed_model,
                cfg.embedding.dim,
                cfg.scan.redact_at_index,
                &mut wdog,
                &watchdog_spec,
                watchdog_headroom,
                ctx_llm
                    .as_ref()
                    .map(|l| l as &(dyn indexa_llm::Describer + Send + Sync)),
            )
            .await?;
        }

        if show_progress {
            eprint!("\r\x1b[K"); // clear the progress line
            let _ = std::io::stderr().flush();
        }
        if skipped > 0 {
            println!("  skipped {skipped}/{} files (unchanged)", files.len());
        }
        if no_embed {
            println!("  indexed {total_chunks} new chunks (FTS only, no embeddings).");
        } else if summary_mode == SummaryMode::SummariesOnly {
            println!(
                "  parsed {total_chunks} chunks (summaries-only — not stored; entries + code graph updated)."
            );
        } else {
            println!("  embedded {total_chunks} new chunks.");
        }
    }

    // Enqueue summarization for every mode — `summaries-only` needs this MOST, since the
    // summary is the only artifact this mode produces (no chunks/embeddings are stored above).
    for root in &roots {
        match enqueue_subtree(&mut store, root) {
            Ok(n) if n > 0 => println!(
                "  enqueued {n} items for background summarization. Run `indexa worker` or use the web UI."
            ),
            Ok(_) => {}
            Err(e) => println!("  warning: failed to enqueue summaries: {e}"),
        }
    }

    // Agent-session content-scope (post-pass, decoupled from the per-file loop above): re-check
    // .jsonl/.ndjson entries against the content-sniffed AgentSessionParser and stamp
    // entries.agent_session so `search`/`ask` can scope to transcript content via
    // `category:agent-session` (see docs/how-to/index-agent-session-history.md). Lives HERE
    // (not duplicated at each CLI call site) so every real `cmd_deep` completion — `indexa deep`
    // directly, and every command that calls `cmd_deep` internally (`indexa index`,
    // `indexa notes add`, `indexa pack refresh`) — gets it automatically. Fail-open: never turn
    // a successful `deep` run into a failed command.
    if let Err(e) = indexa_query::session_scope::tag_agent_session_entries(&mut store) {
        tracing::warn!("agent-session content tagging failed: {e:#}");
    }

    println!("\nDeep index done. Run `indexa ask \"<question>\"` to query.");
    Ok(())
}

/// Estimates `deep --dry-run`'s chunk count for a file population: exactly for a small tree, or
/// via an evenly-spaced sample extrapolated by chunks-per-byte for a large one.
///
/// File size alone is a poor predictor of chunk count — code chunks per function, far
/// finer-grained than prose — so a pure size heuristic can underestimate a real corpus by
/// multiples. Sampling actual parses and extrapolating their chunks-per-byte ratio to the whole
/// tree is the accurate-yet-cheap middle ground: a `deep --dry-run` preview no longer has to
/// nearly-fully parse a huge tree (the cost it exists to help someone avoid) just to report a
/// count.
///
/// `chunks_of` is injected (rather than this function calling the real parser registry directly)
/// so the selection + extrapolation logic stays pure and unit-testable over a synthetic
/// population; production wires it to `registry.parse_guarded`.
///
/// Returns `(estimated_total_chunks, files_actually_parsed, was_sampled)`. A tree at or below
/// `sample_max` files is parsed in full — the "sample" and "everything" are the same set, so the
/// count is exact and `was_sampled` is `false`.
fn estimate_total_chunks(
    files: &[(std::path::PathBuf, u64)],
    sample_max: usize,
    max_parse_bytes: u64,
    mut chunks_of: impl FnMut(&std::path::Path, u64) -> Option<usize>,
) -> (usize, usize, bool) {
    // A file over the parse-size cap is truncated at parse time (`parse_guarded` refuses it
    // outright above `max_parse_bytes`), so it never contributes more than that many parseable
    // bytes. Clamp both the sample's and the whole tree's byte totals to match, or a tree with a
    // few oversized files skews the chunks-per-byte ratio.
    let effective_bytes = |sz: u64| {
        if max_parse_bytes == 0 {
            sz
        } else {
            sz.min(max_parse_bytes)
        }
    };

    if files.len() <= sample_max {
        let mut total_chunks = 0usize;
        for (path, size) in files {
            if let Some(n) = chunks_of(path, *size) {
                total_chunks += n;
            }
        }
        return (total_chunks, files.len(), false);
    }

    let step = (files.len() / sample_max).max(1);
    let mut sample_chunks = 0usize;
    let mut sample_bytes = 0u64;
    let mut sampled = 0usize;
    for (path, size) in files.iter().step_by(step) {
        // Accumulated unconditionally — even when the parse below fails — so the sample's byte
        // basis matches `total_bytes`'s below. Gating this on parse success (as the file's chunk
        // count already is) would inflate chunks-per-byte by the unparseable fraction, since the
        // numerator (chunks) and denominator (bytes) would no longer describe the same files.
        sample_bytes += effective_bytes(*size);
        if let Some(n) = chunks_of(path, *size) {
            sample_chunks += n;
        }
        sampled += 1;
    }
    let total_bytes: u64 = files.iter().map(|(_, size)| effective_bytes(*size)).sum();
    let estimated = if sample_bytes == 0 {
        0
    } else {
        ((sample_chunks as f64 / sample_bytes as f64) * total_bytes as f64).round() as usize
    };
    (estimated, sampled, true)
}

#[cfg(test)]
mod tests {
    use super::estimate_total_chunks;
    use std::path::PathBuf;

    /// A synthetic population with real chunks-per-byte VARIANCE (not a fixed ratio) — code-like
    /// files chunk far more densely per byte than prose-like ones — so the extrapolation is
    /// actually exercised, not just algebra that cancels out.
    fn synthetic_population(
        n: usize,
    ) -> (Vec<(PathBuf, u64)>, std::collections::HashMap<usize, usize>) {
        let mut files = Vec::with_capacity(n);
        let mut true_chunks = std::collections::HashMap::new();
        for i in 0..n {
            let path = PathBuf::from(format!("/corpus/file_{i}.txt"));
            // Alternate two size/density profiles so the population isn't uniform.
            let (size, chunks) = if i.is_multiple_of(3) {
                (4_000u64, 12usize) // code-like: dense
            } else {
                (2_000u64, 2usize) // prose-like: sparse
            };
            true_chunks.insert(i, chunks);
            files.push((path, size));
        }
        (files, true_chunks)
    }

    fn chunks_for(
        path: &std::path::Path,
        true_chunks: &std::collections::HashMap<usize, usize>,
    ) -> Option<usize> {
        let idx: usize = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("file_"))
            .and_then(|s| s.parse().ok())
            .expect("synthetic path shape");
        true_chunks.get(&idx).copied()
    }

    #[test]
    fn small_tree_is_parsed_exactly_not_sampled() {
        let (files, truth) = synthetic_population(40);
        let exact_total: usize = truth.values().sum();
        let (estimated, parsed, was_sampled) =
            estimate_total_chunks(&files, 64, 0, |p, _sz| chunks_for(p, &truth));
        assert!(!was_sampled, "40 files with sample_max 64 must not sample");
        assert_eq!(parsed, 40);
        assert_eq!(
            estimated, exact_total,
            "at/below sample_max must be exact, not estimated"
        );
    }

    #[test]
    fn large_tree_estimate_is_within_tolerance_of_true_total() {
        let (files, truth) = synthetic_population(2_000);
        let exact_total: usize = truth.values().sum();
        let (estimated, parsed, was_sampled) =
            estimate_total_chunks(&files, 64, 0, |p, _sz| chunks_for(p, &truth));
        assert!(was_sampled, "2000 files with sample_max 64 must sample");
        // `step_by`'s count is `ceil(len / step)`, and `step` itself is a floor division, so the
        // actual sample can land a few files above `sample_max` (e.g. 65 for this population) —
        // assert "approximately the sample size", not the exact bound, while still catching a
        // sampling failure that degenerates into a full parse (2000 files).
        assert!(
            parsed <= 64 + 8,
            "must sample ~sample_max files, not the whole tree; parsed {parsed}"
        );
        // Don't assert exact equality — that's the whole point of sampling. Assert the estimate
        // lands within a generous tolerance of the true total (real-corpus PR measurements were
        // ~4% off; this synthetic population's alternating profile is a harder case, so allow
        // more headroom while still catching a badly broken extrapolation).
        let tolerance = (exact_total as f64 * 0.20).ceil() as usize;
        let diff = estimated.abs_diff(exact_total);
        assert!(
            diff <= tolerance,
            "estimate {estimated} vs true {exact_total} (diff {diff}) exceeds {tolerance} tolerance"
        );
    }

    #[test]
    fn unparseable_files_dont_inflate_the_estimate() {
        // Half of ALL files (not just sampled ones) always fail to parse (return None) — e.g. a
        // binary misdetected as text, or a format the parser doesn't handle. The correct ground
        // truth here is what running this SAME (partially-failing) parse function over the whole
        // tree would produce, not the idealized "every file parses" total — a consistently
        // failing subset would contribute zero chunks in a real exact run too.
        let (files, truth) = synthetic_population(2_000);
        let flaky_chunks_of = |p: &std::path::Path, _sz: u64| -> Option<usize> {
            let idx: usize = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("file_"))
                .and_then(|s| s.parse().ok())
                .unwrap();
            if idx.is_multiple_of(2) {
                None // simulate a parse failure
            } else {
                truth.get(&idx).copied()
            }
        };
        let full_total: usize = files
            .iter()
            .filter_map(|(p, sz)| flaky_chunks_of(p, *sz))
            .sum();

        let (estimated, _parsed, was_sampled) =
            estimate_total_chunks(&files, 64, 0, flaky_chunks_of);
        assert!(was_sampled);

        // Regression guard: if the byte basis were gated on parse success instead of accumulated
        // unconditionally, chunks-per-byte would be computed over only the successfully-parsed
        // sampled bytes but multiplied by the WHOLE tree's bytes (successes + failures) —
        // roughly doubling the estimate versus `full_total`. The unconditional basis keeps the
        // sample and total byte counts on the same footing, so this should land close.
        let tolerance = (full_total as f64 * 0.35).ceil() as usize; // sampling noise headroom
        let diff = estimated.abs_diff(full_total);
        assert!(
            diff <= tolerance,
            "estimate {estimated} vs full-parse total {full_total} (diff {diff}) exceeds \
             {tolerance} — byte basis likely regressed to gate on parse success"
        );
    }

    #[test]
    fn oversized_files_are_clamped_on_both_sides() {
        // One file is far larger than `max_parse_bytes` on both sample and total sides; without
        // clamping, its huge byte count would swamp the ratio even though `parse_guarded` would
        // refuse to parse more than the cap in the real path.
        let mut files = vec![(PathBuf::from("/corpus/huge.bin"), 10_000_000u64)];
        let mut truth = std::collections::HashMap::new();
        truth.insert(usize::MAX, 0usize); // the huge file never parses (over the cap)
        for i in 0..200 {
            files.push((PathBuf::from(format!("/corpus/file_{i}.txt")), 1_000));
            truth.insert(i, 3);
        }
        let max_parse_bytes = 5_000u64;
        let (estimated, _parsed, was_sampled) =
            estimate_total_chunks(&files, 64, max_parse_bytes, |p, _sz| {
                if p.file_name().unwrap() == "huge.bin" {
                    None // over the cap: parse_guarded would refuse it
                } else {
                    chunks_for(p, &truth)
                }
            });
        assert!(was_sampled);
        // Without clamping, the ~10MB file alone would make the denominator ~200x too large,
        // crushing the estimate toward ~0. With clamping it should land close to the ~600 true
        // chunks from the 200 small files (huge.bin contributes 0 chunks either way).
        let true_total = 200 * 3;
        assert!(
            estimated > true_total / 2,
            "estimate {estimated} collapsed toward zero — oversized file likely wasn't clamped"
        );
    }
}

/// Regression coverage for the CLI `deep` memory-pressure watchdog (previously nonexistent —
/// see `check_deep_watchdog`'s doc comment). Mirrors the technique
/// `crates/web/src/jobs_exec/deep.rs`'s own watchdog-cadence regression test uses
/// (`deep_phase_checks_the_watchdog_per_file_not_only_at_batch_flush`): force deterministic
/// `Pressure::Critical` via `spec.gpu_wired_limit_bytes = 0` (independent of the real test
/// machine's memory) combined with `#[tokio::test(start_paused = true)]` so the recovery-wait
/// loop (capped at `MAX_PAUSE_SECS`) resolves in virtual time instead of real seconds.
///
/// `cmd_deep` itself is large and CLI-config-driven (walks the real filesystem, resolves a
/// config-file-backed index DB path via `require_index_db`), so rather than building a harness
/// around the whole command, this exercises the extracted `check_deep_watchdog` helper
/// directly — the same helper both the per-file call site (before a file registers its misses
/// with the `MissBatcher`) and `flush_deep_batcher` (before `embed_all`) call. A fake
/// `Embedder` counts `unload()` calls, which only happen on a `Pressure::Critical` entry — so
/// calling the helper N times under forced-Critical pressure and seeing exactly N unloads
/// proves it fires on *every* invocation, not just once overall (which is what the CLI's
/// prior — nonexistent — watchdog would have looked like: zero unloads regardless of N).
#[cfg(test)]
mod watchdog_tests {
    use super::check_deep_watchdog;
    use indexa_core::resource::{detect_machine, WatchdogState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts `unload()` calls instead of doing any real embedding work — this test only
    /// exercises the watchdog's pause/unload plumbing, never `embed`/`embed_batch`.
    #[derive(Default)]
    struct CountingEmbedder {
        unloads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl indexa_embed::Embedder for CountingEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            unreachable!("watchdog test never calls embed()")
        }
        fn dim(&self) -> usize {
            8
        }
        async fn unload(&self) {
            self.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn check_deep_watchdog_fires_on_every_call_under_sustained_critical_pressure() {
        let mut spec = detect_machine();
        // Deterministically forces `compute_budget` (and therefore `assess`/`pause_step`) to
        // Critical regardless of the real test machine's memory: `compute_budget` clamps
        // truly-available RAM to `min(available, gpu_wired_limit_bytes)`, so zeroing this
        // ceiling makes the budget <= 0 (and therefore <= -(headroom/2), the Critical
        // threshold) on every sample, headroom = 0 included.
        spec.gpu_wired_limit_bytes = 0;
        let mut wdog = WatchdogState::new();
        let embedder = CountingEmbedder::default();

        // Simulates 3 files each independently checking the watchdog (the per-file call site)
        // plus a 4th standing in for a batch flush — under the pre-fix code there was no such
        // call at all anywhere in `cmd_deep`, so this loop is exactly the coverage that was
        // previously missing.
        for _ in 0..4 {
            check_deep_watchdog(&mut wdog, &spec, 0, Some(&embedder), None).await;
        }

        assert_eq!(
            embedder.unloads.load(Ordering::SeqCst),
            4,
            "watchdog must unload the embedder on every Critical-pressure invocation — proving \
             it fires once per call site, not just once overall"
        );
    }

    #[tokio::test]
    async fn check_deep_watchdog_is_a_noop_under_ok_pressure() {
        // Real machine spec (generous `gpu_wired_limit_bytes`) + zero headroom: on any host
        // with nonzero available RAM, `compute_budget` is positive, so `pause_step` resumes
        // immediately and the helper returns before ever touching the embedder — proving it
        // doesn't unconditionally unload regardless of pressure.
        let spec = detect_machine();
        let mut wdog = WatchdogState::new();
        let embedder = CountingEmbedder::default();

        check_deep_watchdog(&mut wdog, &spec, 0, Some(&embedder), None).await;

        assert_eq!(
            embedder.unloads.load(Ordering::SeqCst),
            0,
            "watchdog must not unload the embedder when pressure is Ok"
        );
    }
}
