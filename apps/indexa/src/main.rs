use anyhow::Result;
use clap::Parser;
use directories::BaseDirs;
use indexa_cli::{Cli, Commands};
use indexa_core::{
    config::{self, Config, HybridMode, SummaryMode},
    store::{ChunkRecord, Store},
    walker::{walk, WalkConfig},
    watcher::{self, ChangeKind, WatcherConfig},
};
use indexa_embed::OllamaEmbedder;
use indexa_llm::OllamaLlm;
use indexa_query::{
    ask, build_tree, enqueue_subtree, render_json, render_markdown, render_xml,
    summarize_subtree_sync, QaConfig,
};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

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
            passes,
        } => cmd_deep(paths, embed_model, dry_run, mode, passes, &cfg).await,
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
        Commands::Status { unknown } => cmd_status(unknown, &cfg).await,
        Commands::Rm { paths, recursive } => cmd_rm(paths, recursive).await,
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
    _passes: Option<u32>,
    cfg: &Config,
) -> Result<()> {
    let summary_mode = match mode.as_str() {
        "compress" => SummaryMode::Compress,
        "summaries-only" => SummaryMode::SummariesOnly,
        _ => SummaryMode::Augment,
    };
    let roots = resolve_roots(paths, false)?;
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();
    let dim = cfg.embedding.dim;

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
                if let Ok(ex) = indexa_parsers::registry::parse(&entry.path) {
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
        let mins = total_chunks.div_ceil(300);
        println!("Estimated time: ~{mins} min (nomic-embed-text @ ~300 chunks/min)");
        return Ok(());
    }

    let mut store = Store::open(&db_path)?;
    let embedder = indexa_embed::from_config(
        &cfg.embedding.provider,
        &embed_model,
        dim,
        &cfg.embedding.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.google.as_deref(),
    )?;

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

        for entry in &files {
            let path_str = entry.path.to_string_lossy().into_owned();

            // Skip-if-unchanged: re-embedding is expensive; skip files whose chunks
            // are already indexed at or after the file's last modification time.
            if store.chunks_are_current(&path_str).unwrap_or(false) {
                skipped += 1;
                continue;
            }

            let extracted = match indexa_parsers::registry::parse(&entry.path) {
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
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let store = Store::open(&db_path)?;
    let total = store.entry_count()?;
    let chunks = store.chunk_count()?;
    let summary = store.region_summary()?;

    println!("Indexa map — {total} entries, {chunks} deep-scanned chunks (depth ≤{depth})\n");
    println!("{:<20} {:>10} {:>14}", "Category", "Files", "Size");
    println!("{}", "-".repeat(46));
    for r in summary {
        println!(
            "{:<20} {:>10} {:>14}",
            r.category,
            r.entry_count,
            format_size(r.total_size)
        );
    }
    Ok(())
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
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let store = Store::open(&db_path)?;
    let chunk_count = store.chunk_count()?;
    if chunk_count == 0 {
        println!("No deep-scanned content found. Run `indexa deep <path>` first.");
        return Ok(());
    }

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();
    let llm_model = llm_model_flag
        .as_deref()
        .unwrap_or(&cfg.describer.model)
        .to_owned();
    let dim = cfg.embedding.dim;

    let embedder = indexa_embed::from_config(
        &cfg.embedding.provider,
        &embed_model,
        dim,
        &cfg.embedding.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.google.as_deref(),
    )?;
    let llm = indexa_llm::from_config(
        &cfg.describer.provider,
        &llm_model,
        &cfg.describer.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.anthropic.as_deref(),
    )?;

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
        rrf_k: cfg.retrieval.rrf_k as f32,
        ..QaConfig::default()
    };

    let answer = ask(&store, embedder.as_ref(), llm.as_ref(), &question, &qa_cfg).await?;

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
    let dim = cfg.embedding.dim;

    let embedder = indexa_embed::from_config(
        &cfg.embedding.provider,
        &embed_model,
        dim,
        &cfg.embedding.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.google.as_deref(),
    )?;

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
                    let extracted = match indexa_parsers::registry::parse(path) {
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
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let store = indexa_core::store::Store::open(&db_path)?;

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();
    let llm_model = llm_model_flag
        .as_deref()
        .unwrap_or(&cfg.describer.model)
        .to_owned();
    let dim = cfg.embedding.dim;

    let embedder: std::sync::Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(indexa_embed::from_config(
            &cfg.embedding.provider,
            &embed_model,
            dim,
            &cfg.embedding.base_url,
            cfg.api_keys.openai.as_deref(),
            cfg.api_keys.google.as_deref(),
        )?);
    let llm: std::sync::Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(indexa_llm::from_config(
            &cfg.describer.provider,
            &llm_model,
            &cfg.describer.base_url,
            cfg.api_keys.openai.as_deref(),
            cfg.api_keys.anthropic.as_deref(),
        )?);

    indexa_web::serve(port, store, embedder, llm, cfg.clone()).await
}

async fn cmd_status(show_unknown: bool, cfg: &Config) -> Result<()> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let store = Store::open(&db_path)?;
    let entries = store.entry_count()?;
    let chunks = store.chunk_count()?;
    let embedded = store.embedded_chunk_count()?;
    let last_ts = store.last_indexed_at()?;
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    let config_path = config::default_config_path().to_string_lossy().into_owned();

    println!("Index:    {} ({})", db_path.display(), format_size(db_size));

    // Count files vs dirs
    let dirs = {
        let store2 = Store::open(&db_path)?;
        let all = store2.entry_count()?;
        // files = entries that are not dirs
        let _ = all;
        0u64 // placeholder — we don't track dir vs file count separately in a single query
    };
    let _ = dirs;
    println!("Entries:  {entries} total");
    println!(
        "Chunks:   {} ({embedded} embedded with {})",
        chunks, cfg.embedding.model
    );

    if let Some(ts) = last_ts {
        use std::time::{Duration, UNIX_EPOCH};
        let dt = UNIX_EPOCH + Duration::from_secs(ts as u64);
        let secs = dt
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Simple timestamp formatting without chrono
        println!("Last indexed: unix timestamp {secs}");
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
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let mut summary_cfg = cfg.describer.clone();
    summary_cfg.mode = match mode.as_str() {
        "compress" => SummaryMode::Compress,
        "summaries-only" => SummaryMode::SummariesOnly,
        _ => SummaryMode::Augment,
    };

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
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

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
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }
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
        std::fs::write(&path, &out_buf)?;
        println!("Wrote {} bytes to {path}.", out_buf.len());
    } else {
        print!("{out_buf}");
    }

    Ok(())
}

async fn cmd_worker(concurrency: usize, cfg: &Config) -> Result<()> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

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

    let stats = store.lock().await.queue_stats()?;
    println!(
        "Summary worker starting ({concurrency} concurrent). Queue: {} pending, {} done, {} failed.",
        stats.pending, stats.done, stats.failed
    );
    println!("Press Ctrl-C to stop.");

    let summary_cfg = cfg.describer.clone();
    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let s = Arc::clone(&store);
        let d = Arc::clone(&describer);
        let e = Arc::clone(&embedder);
        let c = summary_cfg.clone();
        handles.push(tokio::spawn(indexa_query::run_worker(s, d, e, c)));
    }

    // Wait for all (runs forever until Ctrl-C)
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
