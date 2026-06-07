use anyhow::Result;
use indexa_core::{config, config::Config, store::Store};
use serde::Serialize;

use super::helpers::{format_size, format_unix_timestamp, require_index_db};

#[derive(Serialize)]
struct QueueJson {
    pending: i64,
    in_flight: i64,
    failed: i64,
}

#[derive(Serialize)]
struct StatusJson {
    version: String,
    index_path: String,
    index_bytes: u64,
    entries: u64,
    chunks: u64,
    embedded_chunks: u64,
    last_indexed_at: Option<i64>,
    summaries: u64,
    queue: QueueJson,
    config_path: String,
    embedding_provider: String,
    embedding_model: String,
    embedding_dim: usize,
    describer_provider: String,
    describer_model: String,
    describer_file_model: String,
    describer_dir_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unknown_extensions: Option<Vec<UnknownExtJson>>,
}

#[derive(Serialize)]
struct UnknownExtJson {
    extension: String,
    count: u64,
}

pub(crate) async fn cmd_status(show_unknown: bool, json: bool, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = Store::open(&db_path)?;
    let entries = store.entry_count()?;
    let chunks = store.chunk_count()?;
    let embedded = store.embedded_chunk_count()?;
    let last_ts = store.last_indexed_at()?;
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let summary_count = store.summary_count().unwrap_or(0);
    let queue = store.queue_stats().unwrap_or_default();
    let config_path = config::default_config_path().to_string_lossy().into_owned();

    if json {
        let unknown_extensions = if show_unknown {
            Some(
                store
                    .unknown_extensions(20)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(extension, count)| UnknownExtJson { extension, count })
                    .collect(),
            )
        } else {
            None
        };
        let out = StatusJson {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            index_path: db_path.display().to_string(),
            index_bytes: db_size,
            entries,
            chunks,
            embedded_chunks: embedded,
            last_indexed_at: last_ts,
            summaries: summary_count,
            queue: QueueJson {
                pending: queue.pending,
                in_flight: queue.in_flight,
                failed: queue.failed,
            },
            config_path,
            embedding_provider: cfg.embedding.provider.clone(),
            embedding_model: cfg.embedding.model.clone(),
            embedding_dim: cfg.embedding.dim,
            describer_provider: cfg.describer.provider.clone(),
            describer_model: cfg.describer.model.clone(),
            describer_file_model: cfg.describer.file_model.clone(),
            describer_dir_model: cfg.describer.dir_model.clone(),
            unknown_extensions,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("Indexa:   v{}", env!("CARGO_PKG_VERSION"));
    println!("Index:    {} ({})", db_path.display(), format_size(db_size));
    println!("Entries:  {entries} total");
    println!(
        "Chunks:   {} ({embedded} embedded with {})",
        chunks, cfg.embedding.model
    );

    if let Some(ts) = last_ts {
        println!("Last indexed: {}", format_unix_timestamp(ts));
    }

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
