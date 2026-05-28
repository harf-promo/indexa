use anyhow::Result;
use clap::Parser;
use directories::BaseDirs;
use indexa_cli::{Cli, Commands};
use indexa_core::{
    config::{self, Config, HybridMode},
    store::{ChunkRecord, Store},
    walker::{walk, WalkConfig},
    watcher::{self, ChangeKind, WatcherConfig},
};
use indexa_embed::{Embedder as _, OllamaEmbedder};
use indexa_llm::OllamaLlm;
use indexa_query::{ask, QaConfig};
use std::path::PathBuf;

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
        } => cmd_deep(paths, embed_model, dry_run, &cfg).await,
        Commands::Map { depth } => cmd_map(depth).await,
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
        Commands::Status => cmd_status(&cfg).await,
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
        let count = entries.len();
        store.upsert_entries(&entries)?;
        println!("  indexed {count} entries");
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
    cfg: &Config,
) -> Result<()> {
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
    let base_url = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
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
    let embedder = OllamaEmbedder::new(&base_url, &embed_model, dim);

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

        for entry in &files {
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
                    entry_path: entry.path.to_string_lossy().into_owned(),
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

        println!("  embedded {total_chunks} chunks.");
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
    let embed_base_url = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let llm_base_url = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
    let dim = cfg.embedding.dim;

    let embedder = OllamaEmbedder::new(&embed_base_url, &embed_model, dim);
    let llm = OllamaLlm::new(&llm_base_url, &llm_model);

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

    let answer = ask(&store, &embedder, &llm, &question, &qa_cfg).await?;

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
    let base_url = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let dim = cfg.embedding.dim;

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
        let embedder = indexa_embed::OllamaEmbedder::new(&base_url, &embed_model, dim);
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
                            let embedding = indexa_embed::Embedder::embed(&embedder, &chunk.text)
                                .await
                                .ok();
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
    let base_url = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let llm_base_url = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
    let dim = cfg.embedding.dim;

    let embedder: std::sync::Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        std::sync::Arc::new(OllamaEmbedder::new(&base_url, &embed_model, dim));
    let llm: std::sync::Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        std::sync::Arc::new(OllamaLlm::new(&llm_base_url, &llm_model));

    indexa_web::serve(port, store, embedder, llm, cfg.clone()).await
}

async fn cmd_status(cfg: &Config) -> Result<()> {
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

    println!();
    println!("Config:   {config_path}");
    println!(
        "Embedding: {} / {} (dim {})",
        cfg.embedding.provider, cfg.embedding.model, cfg.embedding.dim
    );
    println!(
        "Describer: {} / {}",
        cfg.describer.provider, cfg.describer.model
    );

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
    let base =
        BaseDirs::new().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    Ok(base.data_local_dir().join("indexa").join("index.db"))
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
