use anyhow::{Context, Result};
use clap::Parser;
use directories::BaseDirs;
use indexa_cli::{Cli, Commands};
use indexa_core::{
    config::{self, Config, HybridMode, SummaryMode},
    resource::{
        detect_machine, estimate_eta, format_duration_pub, lookup_footprint, sample_memory_once,
        ResourceProfile,
    },
    store::{ChunkRecord, Store},
    walker::{walk, WalkConfig},
    watcher::{self, ChangeKind, WatcherConfig},
};
use indexa_embed::OllamaEmbedder;
use indexa_llm::OllamaLlm;
use indexa_query::{
    answer, build_tree, enqueue_subtree, render_json, render_markdown, render_xml,
    summarize_subtree_sync, QaConfig,
};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Determine log directory: <data_dir>/logs/ or a fallback temp path.
    let log_dir = indexa_core::config::default_data_dir()
        .map(|d| d.join("logs"))
        .unwrap_or_else(|| std::env::temp_dir().join("indexa-logs"));
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "indexa.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(tracing::Level::INFO.into());

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(env_filter.clone()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_filter(env_filter),
        )
        .init();

    // Panic hook: capture backtraces to the log file before crashing.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<unknown>");
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown location>".to_owned());
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = msg, location = %location, backtrace = %bt, "indexa panicked");
    }));

    let cli = Cli::parse();

    let cfg = if let Some(path) = &cli.config {
        let expanded = shellexpand::tilde(path).into_owned();
        config::load(std::path::Path::new(&expanded))?
    } else {
        config::load_default()?
    };

    match cli.command {
        Commands::Scan { paths, all } => cmd_scan(paths, all).await,
        Commands::Deep {
            paths,
            embed_model,
            dry_run,
            mode,
        } => cmd_deep(paths, embed_model, dry_run, mode, &cfg).await,
        Commands::Map { depth } => cmd_map(depth).await,
        Commands::Summarize {
            paths,
            mode,
            passes,
        } => cmd_summarize(paths, mode, passes, &cfg).await,
        Commands::Describe { path } => cmd_describe(path).await,
        Commands::Worker { concurrency } => cmd_worker(concurrency, &cfg).await,
        Commands::Export {
            paths,
            format,
            depth,
            output,
        } => cmd_export(paths, format, depth, output).await,
        Commands::Ask {
            question,
            embed_model,
            llm_model,
            scope,
            top_k,
            sparse_only,
            dense_only,
        } => {
            cmd_ask(
                question,
                embed_model,
                llm_model,
                scope,
                top_k,
                sparse_only,
                dense_only,
                &cfg,
            )
            .await
        }
        Commands::Watch { paths, embed_model } => cmd_watch(paths, embed_model, &cfg).await,
        Commands::Serve {
            port,
            embed_model,
            llm_model,
        } => cmd_serve(port, embed_model, llm_model, &cfg).await,
        Commands::Mcp {} => cmd_mcp(&cfg).await,
        Commands::Status { unknown } => cmd_status(unknown, &cfg).await,
        Commands::Rm { paths, recursive } => cmd_rm(paths, recursive).await,
        Commands::Doctor {
            profile,
            files,
            chunks,
        } => cmd_doctor(profile, files, chunks).await,
    }
}

async fn cmd_scan(paths: Vec<String>, all: bool) -> Result<()> {
    let roots = resolve_roots(paths, all)?;
    let db_path = index_db_path()?;
    let mut store = Store::open(&db_path)?;

    for root in &roots {
        println!("Scanning {}", root.display());
        let entries = walk(root, &WalkConfig::default())?;
        let live_paths: std::collections::HashSet<String> = entries
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();

        store.upsert_entries(&entries)?;

        // Ghost-row cleanup: remove entries that were in the index but no longer on disk.
        let root_str = root.to_string_lossy().into_owned();
        let removed = store.reconcile_entries(&root_str, &live_paths)?;
        let count = live_paths.len();
        if removed > 0 {
            println!("  {count} entries, removed {removed} ghost rows");
        } else {
            println!("  {count} entries");
        }
    }

    println!("\nIndex saved to {}", db_path.display());
    println!("Run `indexa map` to see a summary.");
    println!("Run `indexa deep <path>` to parse and embed file contents.");
    Ok(())
}

async fn cmd_deep(
    paths: Vec<String>,
    embed_model_flag: Option<String>,
    dry_run: bool,
    mode: String,
    cfg: &Config,
) -> Result<()> {
    let summary_mode = parse_summary_mode(&mode)?;
    let roots = resolve_roots(paths, false)?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let max_parse_bytes = cfg.parsers.max_file_mb.saturating_mul(1024 * 1024);

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();

    if dry_run {
        println!("Dry run — nothing will be written to the index.\n");
        let mut total_files = 0usize;
        let mut total_chunks = 0usize;
        let mut by_mime: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for root in &roots {
            let entries = walk(root, &WalkConfig::default())?;
            let files: Vec<_> = entries
                .iter()
                .filter(|e| e.kind == indexa_core::walker::EntryKind::File)
                .collect();
            total_files += files.len();
            for entry in files {
                if let Ok(ex) = indexa_parsers::registry::parse_guarded(
                    &entry.path,
                    entry.size,
                    max_parse_bytes,
                ) {
                    total_chunks += ex.chunks.len();
                    let family = ex.mime.split('/').next().unwrap_or("other").to_owned();
                    *by_mime.entry(family).or_default() += 1;
                }
            }
        }

        println!("Would parse {total_files} files:");
        let mut pairs: Vec<_> = by_mime.into_iter().collect();
        pairs.sort_by_key(|b| std::cmp::Reverse(b.1));
        for (mime, n) in pairs {
            println!("  {:>5}  {mime}", n);
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
    let embedder = build_embedder(cfg, Some(&embed_model))?;

    for root in &roots {
        println!(
            "Deep-scanning {} with embed model '{}'",
            root.display(),
            embed_model
        );
        let entries = walk(root, &WalkConfig::default())?;
        let files: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == indexa_core::walker::EntryKind::File)
            .collect();

        println!("  parsing {} files...", files.len());
        let mut total_chunks = 0usize;
        let mut skipped = 0usize;

        // Lightweight in-place progress on stderr (carriage-return rewrite), shown only when
        // stderr is a terminal so piped/CI output stays clean. Hand-rolled to avoid pulling in
        // indicatif, whose transitive `number_prefix` dep is flagged unmaintained (RUSTSEC-2025-0119).
        let show_progress = std::io::stderr().is_terminal();
        let total_files = files.len();

        for (i, entry) in files.iter().enumerate() {
            if show_progress {
                let name = entry
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                eprint!("\r\x1b[K  [{}/{total_files}] {:.50}", i + 1, name);
                let _ = std::io::stderr().flush();
            }
            let path_str = entry.path.to_string_lossy().into_owned();

            // Skip-if-unchanged: re-embedding is expensive; skip files whose chunks
            // are already indexed at or after the file's last modification time.
            if store.chunks_are_current(&path_str).unwrap_or(false) {
                skipped += 1;
                continue;
            }

            let extracted = match indexa_parsers::registry::parse_guarded(
                &entry.path,
                entry.size,
                max_parse_bytes,
            ) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if extracted.chunks.is_empty() {
                continue;
            }

            let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
            for chunk in &extracted.chunks {
                let embedding = embedder.embed(&chunk.text).await.ok();
                chunk_records.push(ChunkRecord {
                    entry_path: path_str.clone(),
                    seq: chunk.seq,
                    heading: chunk.heading.clone(),
                    text: chunk.text.clone(),
                    language: chunk.language.clone(),
                    embedding,
                    embed_model: Some(embed_model.clone()),
                });
            }

            store.upsert_chunks(&chunk_records)?;
            total_chunks += chunk_records.len();
        }

        if show_progress {
            eprint!("\r\x1b[K"); // clear the progress line
            let _ = std::io::stderr().flush();
        }
        if skipped > 0 {
            println!("  skipped {skipped}/{} files (unchanged)", files.len());
        }
        println!("  embedded {total_chunks} new chunks.");
    }

    // Enqueue summarization for non-Augment modes or always to populate the queue
    if summary_mode != SummaryMode::SummariesOnly {
        for root in &roots {
            match enqueue_subtree(&mut store, root) {
                Ok(n) if n > 0 => println!(
                    "  enqueued {n} items for background summarization. Run `indexa worker` or use the web UI."
                ),
                Ok(_) => {}
                Err(e) => println!("  warning: failed to enqueue summaries: {e}"),
            }
        }
    }

    println!("\nDeep index done. Run `indexa ask \"<question>\"` to query.");
    Ok(())
}

async fn cmd_map(depth: usize) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = Store::open(&db_path)?;
    let total = store.entry_count()?;
    let chunks = store.chunk_count()?;
    let summary = store.region_summary()?;

    // Only emit ANSI escapes when stdout is a terminal, so piping/redirecting stays clean.
    let color = std::io::stdout().is_terminal();
    let sgr = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };

    println!(
        "{}",
        sgr(
            "1",
            &format!("Indexa map — {total} entries, {chunks} deep-scanned chunks (depth ≤{depth})")
        )
    );
    println!();
    println!(
        "{}",
        sgr(
            "1",
            &format!("{:<20} {:>10} {:>14}", "Category", "Files", "Size")
        )
    );
    println!("{}", sgr("2", &"-".repeat(46)));
    for r in summary {
        // Pad first (ANSI codes don't count toward display width), then colorize.
        let cat = sgr(category_color(&r.category), &format!("{:<20}", r.category));
        println!(
            "{cat} {:>10} {:>14}",
            r.entry_count,
            format_size(r.total_size)
        );
    }
    Ok(())
}

/// ANSI SGR color code for a surface-scan category (used by `indexa map`).
fn category_color(category: &str) -> &'static str {
    match category {
        "code" => "36",            // cyan
        "documents" => "34",       // blue
        "media" => "35",           // magenta
        "cache" | "build" => "33", // yellow
        "system" => "90",          // bright black
        "unknown" => "37",         // white
        _ => "32",                 // green
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_ask(
    question: String,
    embed_model_flag: Option<String>,
    llm_model_flag: Option<String>,
    scope_flag: Option<String>,
    top_k_flag: Option<usize>,
    sparse_only: bool,
    dense_only: bool,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = Store::open(&db_path)?;
    let chunk_count = store.chunk_count()?;
    if chunk_count == 0 {
        println!("No deep-scanned content found. Run `indexa deep <path>` first.");
        return Ok(());
    }

    let embedder = build_embedder(cfg, embed_model_flag.as_deref())?;
    let llm = build_llm(cfg, llm_model_flag.as_deref())?;

    let mode = if sparse_only {
        HybridMode::Sparse
    } else if dense_only {
        HybridMode::Dense
    } else {
        cfg.retrieval.hybrid.clone()
    };

    let scope = scope_flag
        .as_deref()
        .map(|s| shellexpand::tilde(s).into_owned());

    println!("Searching {chunk_count} indexed chunks...\n");

    let qa_cfg = QaConfig {
        top_k: top_k_flag.unwrap_or(cfg.retrieval.top_k),
        mode,
        scope,
        context_budget: cfg.retrieval.context_budget,
        rrf_k: cfg.retrieval.rrf_k as f32,
        summary_weight: cfg.retrieval.summary_weight,
        summary_depth_alpha: cfg.retrieval.summary_depth_alpha,
        rerank: cfg.retrieval.rerank,
    };

    // `store` is no longer needed by the query path — `answer` opens its own
    // scoped connection. Drop it so we don't hold two handles open.
    drop(store);
    let answer = answer(
        &db_path,
        embedder.as_ref(),
        llm.as_ref(),
        &question,
        &qa_cfg,
    )
    .await?;

    println!("Answer:\n{}\n", answer.answer);

    if !answer.sources.is_empty() {
        println!("Sources:");
        for (i, src) in answer.sources.iter().enumerate() {
            let loc = if src.heading.is_empty() {
                src.path.clone()
            } else {
                format!("{} — {}", src.path, src.heading)
            };
            println!("  [{}] {}", i + 1, loc);
        }
    }

    Ok(())
}

async fn cmd_watch(
    paths: Vec<String>,
    embed_model_flag: Option<String>,
    cfg: &Config,
) -> Result<()> {
    let roots = resolve_roots(paths, false)?;
    let db_path = index_db_path()?;

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();

    let embedder = build_embedder(cfg, Some(&embed_model))?;

    println!(
        "Watching {} path(s) for changes. Press Ctrl-C to stop.",
        roots.len()
    );
    for r in &roots {
        println!("  {}", r.display());
    }
    println!();

    let session = watcher::watch(&roots, &WatcherConfig::default())?;

    let db_path_clone = db_path.clone();
    let max_parse_bytes = cfg.parsers.max_file_mb.saturating_mul(1024 * 1024);
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();

        watcher::run_watch_loop(session, |event| {
            let path = &event.path;
            if path.is_dir() {
                return;
            }
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false)
            {
                return;
            }

            match event.kind {
                ChangeKind::Remove => {
                    if let Ok(mut store) = Store::open(&db_path_clone) {
                        let path_str = path.to_string_lossy().into_owned();
                        if let Err(e) = store.delete_chunks_for(&path_str) {
                            tracing::warn!("failed to delete chunks for {path_str}: {e}");
                        } else {
                            println!("  removed: {path_str}");
                        }
                    }
                }
                ChangeKind::Upsert => {
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    let extracted = match indexa_parsers::registry::parse_guarded(
                        path,
                        size,
                        max_parse_bytes,
                    ) {
                        Ok(e) => e,
                        Err(_) => return,
                    };
                    if extracted.chunks.is_empty() {
                        return;
                    }

                    let chunk_records: Vec<ChunkRecord> = rt.block_on(async {
                        let mut records = Vec::with_capacity(extracted.chunks.len());
                        for chunk in &extracted.chunks {
                            let embedding = embedder.embed(&chunk.text).await.ok();
                            records.push(ChunkRecord {
                                entry_path: path.to_string_lossy().into_owned(),
                                seq: chunk.seq,
                                heading: chunk.heading.clone(),
                                text: chunk.text.clone(),
                                language: chunk.language.clone(),
                                embedding,
                                embed_model: Some(embed_model.clone()),
                            });
                        }
                        records
                    });

                    if let Ok(mut store) = Store::open(&db_path_clone) {
                        if let Err(e) = store.upsert_chunks(&chunk_records) {
                            tracing::warn!("failed to upsert chunks for {}: {e}", path.display());
                        } else {
                            println!(
                                "  re-indexed: {} ({} chunks)",
                                path.display(),
                                chunk_records.len()
                            );
                        }
                    }
                }
            }
        });
    })
    .await?;

    Ok(())
}

async fn cmd_serve(
    port: u16,
    embed_model_flag: Option<String>,
    llm_model_flag: Option<String>,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = indexa_core::store::Store::open(&db_path)?;

    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(build_embedder(cfg, embed_model_flag.as_deref())?);
    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(build_llm(cfg, llm_model_flag.as_deref())?);

    indexa_web::serve(port, store, embedder, llm, cfg.clone()).await
}

/// Run the MCP (Model Context Protocol) server over stdio so AI agents
/// (Claude Desktop, Cursor, …) can browse the index live as tool calls.
/// stdout is the JSON-RPC channel — Indexa's tracing already writes to stderr only.
async fn cmd_mcp(cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(build_embedder(cfg, None)?);
    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(build_llm(cfg, None)?);
    indexa_mcp::serve_mcp(db_path, embedder, llm, cfg.clone()).await
}

async fn cmd_status(show_unknown: bool, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = Store::open(&db_path)?;
    let entries = store.entry_count()?;
    let chunks = store.chunk_count()?;
    let embedded = store.embedded_chunk_count()?;
    let last_ts = store.last_indexed_at()?;
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    let config_path = config::default_config_path().to_string_lossy().into_owned();

    println!("Index:    {} ({})", db_path.display(), format_size(db_size));
    println!("Entries:  {entries} total");
    println!(
        "Chunks:   {} ({embedded} embedded with {})",
        chunks, cfg.embedding.model
    );

    if let Some(ts) = last_ts {
        println!("Last indexed: {}", format_unix_timestamp(ts));
    }

    let summary_count = store.summary_count().unwrap_or(0);
    let queue = store.queue_stats().unwrap_or_default();
    println!(
        "Summaries: {} (queue: {} pending / {} in-flight / {} failed)",
        summary_count, queue.pending, queue.in_flight, queue.failed
    );

    println!();
    println!("Config:   {config_path}");
    println!(
        "Embedding: {} / {} (dim {})",
        cfg.embedding.provider, cfg.embedding.model, cfg.embedding.dim
    );
    println!(
        "Describer: {} / {} (file: {}, dir: {})",
        cfg.describer.provider,
        cfg.describer.model,
        cfg.describer.file_model,
        cfg.describer.dir_model
    );

    if show_unknown {
        println!();
        println!("Top unclassified file extensions:");
        match store.unknown_extensions(20) {
            Ok(rows) if rows.is_empty() => println!("  (none — all files classified)"),
            Ok(rows) => {
                for (ext, n) in rows {
                    println!("  {:>5}  {ext}", n);
                }
            }
            Err(e) => println!("  (error: {e})"),
        }
    }

    Ok(())
}

async fn cmd_rm(paths: Vec<String>, recursive: bool) -> Result<()> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found.");
        return Ok(());
    }

    let mut store = Store::open(&db_path)?;
    let mut total_removed = 0usize;

    for path_str in &paths {
        let expanded = shellexpand::tilde(path_str).into_owned();
        if recursive {
            let n = store.delete_subtree(&expanded)?;
            total_removed += n;
            println!("Removed subtree: {expanded} ({n} entries)");
        } else {
            let n = store.delete_entry(&expanded)?;
            total_removed += n;
            if n > 0 {
                println!("Removed: {expanded}");
            } else {
                println!("Not found in index: {expanded}");
            }
        }
    }

    println!("Total removed: {total_removed} entries");
    Ok(())
}

async fn cmd_summarize(
    paths: Vec<String>,
    mode: String,
    passes: Option<u32>,
    cfg: &Config,
) -> Result<()> {
    let roots = resolve_roots(paths, false)?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let mut summary_cfg = cfg.describer.clone();
    summary_cfg.mode = parse_summary_mode(&mode)?;

    let base_url = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
    let describer = OllamaLlm::new_with_dir_model(
        &base_url,
        &cfg.describer.file_model,
        &cfg.describer.dir_model,
    );
    let embed_base = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let embedder = OllamaEmbedder::new(&embed_base, &cfg.embedding.model, cfg.embedding.dim);

    let mut store = Store::open(&db_path)?;

    for root in &roots {
        println!("Summarizing {} …", root.display());
        let done = summarize_subtree_sync(
            &mut store,
            &describer,
            &embedder,
            root,
            &summary_cfg,
            passes,
        )
        .await?;
        println!("  {done} summaries written.");
    }

    Ok(())
}

async fn cmd_describe(path: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let expanded = shellexpand::tilde(&path).into_owned();
    let store = Store::open(&db_path)?;

    match store.summary_by_path(&expanded)? {
        None => println!("No summary found for {expanded}. Run `indexa summarize` first."),
        Some(rec) => {
            // Print breadcrumb chain
            let crumbs = store.ancestor_summaries(&expanded)?;
            if !crumbs.is_empty() {
                let chain: Vec<&str> = crumbs.iter().map(|c| c.path.as_str()).collect();
                println!("Breadcrumb: {}", chain.join(" › "));
                println!();
                for crumb in &crumbs {
                    let name = std::path::Path::new(&crumb.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| crumb.path.clone());
                    println!("  {name}: {}", crumb.summary);
                }
                println!();
            }

            let kind_icon = if rec.kind == "dir" { "📁" } else { "📄" };
            println!("{kind_icon} {expanded}");
            println!("  Model:  {}", rec.model);
            println!("  Kind:   {}", rec.kind);
            if let Some(ref abstract_) = rec.summary_l0 {
                println!("  Abstract: {abstract_}");
            }
            println!();
            println!("{}", rec.summary);

            // Show immediate children if directory
            if rec.kind == "dir" {
                let children = store.children_summaries(&expanded)?;
                if !children.is_empty() {
                    println!("\nChildren ({}):", children.len());
                    for child in children.iter().take(20) {
                        let name = std::path::Path::new(&child.path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| child.path.clone());
                        let icon = if child.kind == "dir" { "📁" } else { "📄" };
                        println!("  {icon} {name}: {}", child.summary);
                    }
                }
            }
        }
    }

    Ok(())
}

async fn cmd_export(
    paths: Vec<String>,
    format: String,
    depth: Option<usize>,
    output: Option<String>,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let count = store.summary_count()?;
    if count == 0 {
        println!("No summaries found. Run `indexa summarize <path>` first.");
        return Ok(());
    }

    let roots: Vec<String> = if paths.is_empty() {
        // Export the roots of the summary tree (depth = 0).
        store
            .tree_level("")
            .unwrap_or_default()
            .into_iter()
            .map(|n| n.path)
            .collect()
    } else {
        paths
            .into_iter()
            .map(|p| shellexpand::tilde(&p).into_owned())
            .collect()
    };

    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_owned())
    };

    let mut out_buf = String::new();
    for root_path in &roots {
        let tree = build_tree(&store, root_path, depth)?;
        let Some(tree) = tree else {
            eprintln!(
                "No summary found for {root_path} — run `indexa summarize {root_path}` first."
            );
            continue;
        };
        let rendered = match format.as_str() {
            "md" | "markdown" => render_markdown(&tree),
            "json" => render_json(&tree),
            _ => render_xml(&tree, &now), // xml is the default
        };
        out_buf.push_str(&rendered);
        out_buf.push('\n');
    }

    if let Some(path) = output {
        // Give an actionable hint when the parent directory doesn't exist, rather
        // than surfacing a bare OS "No such file or directory" error.
        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                anyhow::bail!(
                    "cannot write to '{path}': the directory '{}' does not exist. \
                     Create it first or choose an existing output path.",
                    parent.display()
                );
            }
        }
        std::fs::write(&path, &out_buf).with_context(|| format!("writing export to '{path}'"))?;
        println!("Wrote {} bytes to {path}.", out_buf.len());
    } else {
        print!("{out_buf}");
    }

    Ok(())
}

async fn cmd_worker(concurrency: usize, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let base_url = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
    let describer: Arc<dyn indexa_llm::Describer + Send + Sync> =
        Arc::new(OllamaLlm::new_with_dir_model(
            &base_url,
            &cfg.describer.file_model,
            &cfg.describer.dir_model,
        ));
    let embed_base = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync> = Arc::new(OllamaEmbedder::new(
        &embed_base,
        &cfg.embedding.model,
        cfg.embedding.dim,
    ));

    let store = Arc::new(tokio::sync::Mutex::new(Store::open(&db_path)?));

    // Startup sweep before any worker claims: reset items left `in_flight` by a prior
    // crash/kill back to `pending` (failing those past the attempt cap), so they aren't
    // stranded. Must run before the worker tasks spawn.
    match store.lock().await.requeue_stale_in_flight(3) {
        Ok((requeued, failed)) if requeued > 0 || failed > 0 => println!(
            "Requeued {requeued} stale in-flight item(s) from a previous run ({failed} failed over the attempt cap)."
        ),
        Ok(_) => {}
        Err(e) => eprintln!("Warning: could not sweep stale in-flight items: {e}"),
    }

    let stats = store.lock().await.queue_stats()?;
    println!(
        "Summary worker starting ({concurrency} concurrent). Queue: {} pending, {} done, {} failed.",
        stats.pending, stats.done, stats.failed
    );
    println!("Press Ctrl-C to stop.");

    let summary_cfg = cfg.describer.clone();
    let headroom = cfg.resource.effective_headroom_bytes();
    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let s = Arc::clone(&store);
        let d = Arc::clone(&describer);
        let e = Arc::clone(&embedder);
        let c = summary_cfg.clone();
        handles.push(tokio::spawn(indexa_query::run_worker(s, d, e, c, headroom)));
    }

    // Wait for all (runs forever until Ctrl-C)
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

async fn cmd_doctor(
    profile_str: String,
    files_hint: Option<usize>,
    chunks_hint: Option<usize>,
) -> Result<()> {
    let profile = match profile_str.as_str() {
        "conservative" => ResourceProfile::Conservative,
        "performance" => ResourceProfile::Performance,
        _ => ResourceProfile::Balanced,
    };

    let spec = detect_machine();
    let sample = sample_memory_once();

    let total_gb = spec.total_ram_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let free_gb = sample.free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    // "Reclaimable" = total - actively used (wired+active); macOS's inactive file
    // cache is reclaimable instantly so it counts as available for new allocations.
    let reclaimable_gb = (spec.total_ram_bytes.saturating_sub(sample.used_bytes)) as f64
        / (1024.0 * 1024.0 * 1024.0);
    let wired_limit_gb = spec.gpu_wired_limit_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let headroom_gb = profile.headroom_bytes() as f64 / (1024.0 * 1024.0 * 1024.0);
    use indexa_core::resource::compute_budget;
    let budget_gb = compute_budget(&spec, &sample, profile.headroom_bytes()) as f64
        / (1024.0 * 1024.0 * 1024.0);

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║              indexa doctor — machine profile             ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    // ── Machine spec ──
    println!("Machine");
    if spec.is_apple_silicon {
        println!("  Chip   Apple Silicon (unified memory — CPU+GPU share one pool)");
    } else {
        println!("  Arch   x86-64 / non-Apple");
    }
    // Show reclaimable (total − wired/active) alongside truly-free pages.
    // macOS keeps inactive file cache in "free-looking" RAM; only swap = real pressure.
    println!("  RAM    {total_gb:.0} GB total   {reclaimable_gb:.1} GB reclaimable  ({free_gb:.1} GB truly free)");
    println!(
        "  CPU    {} physical cores, {} logical threads",
        spec.physical_cores, spec.logical_cores
    );
    if spec.is_apple_silicon {
        println!(
            "  GPU    Metal — wired ceiling ≈ {wired_limit_gb:.0} GB ({:.0}% of RAM)",
            wired_limit_gb / total_gb * 100.0
        );
    }
    println!();

    // ── Profile & budget ──
    println!("Resource profile: {}", profile.as_str().to_uppercase());
    println!("  Headroom  {headroom_gb:.0} GB (kept free at all times)");
    println!("  Budget    {budget_gb:.1} GB available for AI models right now");
    println!(
        "  keep_alive  {} s (model stays warm in Ollama between calls)",
        profile.keep_alive_secs()
    );
    println!();

    // ── Ollama env-var check ──
    println!("Ollama server settings");
    let max_loaded = std::env::var("OLLAMA_MAX_LOADED_MODELS").ok();
    let num_parallel = std::env::var("OLLAMA_NUM_PARALLEL").ok();
    let keep_alive_env = std::env::var("OLLAMA_KEEP_ALIVE").ok();

    let check = |name: &str, val: Option<String>, recommended: &str| match val {
        Some(v) => println!("  ✅  {name} = {v}"),
        None => println!("  ⚠️   {name} not set — recommended: {recommended}"),
    };
    check(
        "OLLAMA_MAX_LOADED_MODELS",
        max_loaded,
        "1  (prevents multiple models staying resident)",
    );
    check(
        "OLLAMA_NUM_PARALLEL",
        num_parallel,
        "1  (prevents KV-cache multiplication)",
    );
    check(
        "OLLAMA_KEEP_ALIVE",
        keep_alive_env,
        "30s  (lets models unload between jobs)",
    );
    println!();
    println!("  NOTE: these env vars are read by the Ollama server at startup.");
    println!("  To apply on macOS:");
    println!("    launchctl setenv OLLAMA_MAX_LOADED_MODELS 1");
    println!("    launchctl setenv OLLAMA_NUM_PARALLEL 1");
    println!("    launchctl setenv OLLAMA_KEEP_ALIVE 30s");
    println!("    # then quit and relaunch Ollama.app");
    println!();

    // ── Per-model memory table ──
    println!("Model memory estimates  (num_ctx=4096, num_parallel=1)");
    println!(
        "  {:<28}  {:>10}  {:>8}  {:>6}",
        "Model", "Peak RAM", "Fits?", "Role"
    );
    println!(
        "  {}  {}  {}  {}",
        "─".repeat(28),
        "─".repeat(10),
        "─".repeat(8),
        "─".repeat(20)
    );
    let models_of_interest = [
        ("nomic-embed-text", "embeddings"),
        ("gemma3:4b", "file summaries"),
        ("gemma3:12b", "dir roll-ups / Q&A"),
    ];
    for (model, role) in &models_of_interest {
        let peak_display = lookup_footprint(model)
            .map(|fp| fp.peak_display(4096))
            .unwrap_or_else(|| "unknown".to_owned());
        let fits = lookup_footprint(model)
            .map(|fp| {
                if fp.peak_bytes(4096) as f64 / (1024.0 * 1024.0 * 1024.0) <= budget_gb {
                    "✅"
                } else {
                    "❌"
                }
            })
            .unwrap_or("?");
        println!(
            "  {:<28}  {:>10}  {:>8}  {}",
            model, peak_display, fits, role
        );
    }
    println!();

    // ── Why it freezes (explanation) ──
    println!("Why Indexa can freeze the machine");
    println!("  By default Ollama keeps each model warm for 5 minutes after use.");
    println!("  If nomic-embed-text + gemma3:4b + gemma3:12b all stay resident");
    println!("  at the same time, combined peak can reach 16–20+ GB.  On a");
    println!("  {total_gb:.0} GB machine that pushes into swap → thrash → freeze.");
    println!();
    println!("  The fix Indexa now enforces:");
    println!(
        "    • keep_alive={} s (models unload faster)",
        profile.keep_alive_secs()
    );
    println!("    • num_parallel=1 per request (no KV-cache multiplication)");
    println!("    • Explicit unload when switching between models");
    println!("    • Pre-flight fit check before each job");
    println!();

    // ── ETA estimates ──
    let n_files = files_hint.unwrap_or(200);
    let n_chunks = chunks_hint.unwrap_or(n_files * 8);
    println!(
        "ETA estimates  (for ~{n_files} files / ~{n_chunks} embed chunks, {} passes)",
        2
    );
    println!(
        "  {:<28}  {:>12}  {:>12}  {:>14}",
        "Gen model", "Embed only", "Summarize", "Total (deep+summarize)"
    );
    println!(
        "  {}  {}  {}  {}",
        "─".repeat(28),
        "─".repeat(12),
        "─".repeat(12),
        "─".repeat(14)
    );
    for (model, _role) in &models_of_interest[1..] {
        // skip embed model
        let embed_eta = estimate_eta("nomic-embed-text", 0, n_chunks, 0, 1, spec.is_apple_silicon);
        let sum_eta = estimate_eta(model, n_files, 0, 600, 2, spec.is_apple_silicon);
        let total_secs = embed_eta.total_secs + sum_eta.total_secs;
        println!(
            "  {:<28}  {:>12}  {:>12}  {:>14}",
            model,
            embed_eta.display,
            sum_eta.display,
            format_duration_pub(total_secs as u64),
        );
    }
    println!();
    println!("  Pass `--files N --chunks M` to customise for your index size.");
    println!("  Run `indexa status` to see how many files are currently indexed.");

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the index DB path if it exists, or `None` after printing the standard
/// "no index found" hint. Call sites collapse to:
///
/// ```ignore
/// let Some(db_path) = require_index_db()? else { return Ok(()); };
/// ```
///
/// `cmd_rm` uses a slightly different hint and so opens the DB directly.
fn require_index_db() -> Result<Option<PathBuf>> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(None);
    }
    Ok(Some(db_path))
}

/// Build an embedder from config, optionally overriding the model name.
/// Respects `cfg.resource.effective_keep_alive_secs()` for Ollama.
fn build_embedder(
    cfg: &Config,
    model_override: Option<&str>,
) -> Result<Box<dyn indexa_embed::Embedder + Send + Sync>> {
    let model = model_override.unwrap_or(&cfg.embedding.model);
    let keep_alive = cfg.resource.effective_keep_alive_secs();
    indexa_embed::from_config_with_keep_alive(
        &cfg.embedding.provider,
        model,
        cfg.embedding.dim,
        &cfg.embedding.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.google.as_deref(),
        Some(keep_alive),
    )
}

/// Build an LLM generator from config, optionally overriding the model name.
/// Respects `cfg.resource.effective_keep_alive_secs()` for Ollama.
fn build_llm(
    cfg: &Config,
    model_override: Option<&str>,
) -> Result<Box<dyn indexa_llm::Generator + Send + Sync>> {
    let model = model_override.unwrap_or(&cfg.describer.model);
    let keep_alive = cfg.resource.effective_keep_alive_secs();
    indexa_llm::from_config_with_keep_alive(
        &cfg.describer.provider,
        model,
        &cfg.describer.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.anthropic.as_deref(),
        Some(keep_alive),
    )
}

fn resolve_roots(paths: Vec<String>, all: bool) -> Result<Vec<PathBuf>> {
    if all {
        #[cfg(windows)]
        return Ok(vec![PathBuf::from("C:\\")]);
        #[cfg(not(windows))]
        return Ok(vec![PathBuf::from("/")]);
    }

    if paths.is_empty() {
        let base =
            BaseDirs::new().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        return Ok(vec![base.home_dir().to_path_buf()]);
    }

    paths
        .into_iter()
        .map(|p| {
            let expanded = shellexpand::tilde(&p).into_owned();
            Ok(PathBuf::from(expanded))
        })
        .collect()
}

fn index_db_path() -> Result<PathBuf> {
    let data_dir = config::default_data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    migrate_legacy_data_dir(&data_dir);
    Ok(data_dir.join("index.db"))
}

/// One-time migration: if the old `indexa/` data dir exists but the new canonical
/// `dev.indexa.Indexa/` dir does not, rename it so existing indexes aren't lost.
fn migrate_legacy_data_dir(new_dir: &std::path::Path) {
    if new_dir.exists() {
        return;
    }
    // The old path was `<data_local>/indexa/` (bare name, no qualifier).
    // Derive it by stripping the last component of `new_dir` and appending "indexa".
    if let Some(parent) = new_dir.parent() {
        let old_dir = parent.join("indexa");
        if old_dir.exists() {
            if let Err(e) = std::fs::rename(&old_dir, new_dir) {
                tracing::warn!(
                    "could not migrate data dir {} → {}: {e}",
                    old_dir.display(),
                    new_dir.display()
                );
            } else {
                tracing::info!(
                    "migrated data dir {} → {}",
                    old_dir.display(),
                    new_dir.display()
                );
            }
        }
    }
}

/// Parse the `--mode` flag into a `SummaryMode`, rejecting unknown values with a
/// clear error instead of silently treating a typo (e.g. `compres`) as `augment`.
fn parse_summary_mode(mode: &str) -> Result<SummaryMode> {
    match mode {
        "augment" => Ok(SummaryMode::Augment),
        "compress" => Ok(SummaryMode::Compress),
        "summaries-only" => Ok(SummaryMode::SummariesOnly),
        other => anyhow::bail!(
            "unknown --mode '{other}'. Valid values: augment, compress, summaries-only"
        ),
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a Unix timestamp (seconds since epoch) as a human-readable UTC datetime
/// like `2026-05-29 14:32 UTC`. Uses Howard Hinnant's civil-date algorithm so we
/// avoid pulling in `chrono` just for this one display string.
fn format_unix_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "unknown".to_owned();
    }
    let secs = ts;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute) = (rem / 3_600, (rem % 3_600) / 60);

    // Civil-from-days (Hinnant): days since 1970-01-01 → (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}
