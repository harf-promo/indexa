use anyhow::Result;
use indexa_core::{config, config::Config, store::Store};

use super::helpers::{format_size, format_unix_timestamp, require_index_db};

pub(crate) async fn cmd_status(show_unknown: bool, cfg: &Config) -> Result<()> {
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
