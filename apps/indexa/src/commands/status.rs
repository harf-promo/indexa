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
    // Only when retrieval calls were recorded, so an unused index's output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_week: Option<UsageWeekJson>,
    // Only with --deep, so default `status --json` output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage: Option<CoverageJson>,
}

/// Token-savings telemetry over the last week — bytes, not tokens, so consumers
/// can apply their own tokenizer; the ≈4 chars/token estimate lives in the
/// human-readable line only. See `store::usage` for what counterfactual means.
#[derive(Serialize)]
struct UsageWeekJson {
    calls: u64,
    served: u64,
    counterfactual: u64,
}

#[derive(Serialize)]
struct UnknownExtJson {
    extension: String,
    count: u64,
}

#[derive(Serialize)]
struct CoverageJson {
    files: u64,
    dirs: u64,
    files_with_chunks: u64,
    chunks: u64,
    embedded_chunks: u64,
    files_summarized: u64,
    dirs_summarized: u64,
    stale_summaries: u64,
    open_questions: i64,
    roots: Vec<RootCoverageJson>,
}

#[derive(Serialize)]
struct RootCoverageJson {
    path: String,
    last_indexed_at: Option<i64>,
}

/// `part/whole` as a one-decimal percentage; em dash when there is nothing to
/// measure (avoids a fake "0.0%" on an empty index).
fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        "—".to_owned()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

pub(crate) async fn cmd_status(
    show_unknown: bool,
    deep: bool,
    json: bool,
    cfg: &Config,
) -> Result<()> {
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
    // Token-savings telemetry (best-effort read; absent = no line printed).
    let usage = store
        .usage_summary(indexa_core::store::USAGE_WEEK_SECS)
        .unwrap_or_default();
    let config_path = config::default_config_path().to_string_lossy().into_owned();

    // --deep: one aggregate query plus a per-root timestamp probe (root count
    // is small — these are top-level indexed directories, not files).
    let coverage = if deep {
        let health = store.health_stats()?;
        let open_questions = store.open_decision_count().unwrap_or(0);
        let roots: Vec<(String, Option<i64>)> = store
            .root_paths()
            .unwrap_or_default()
            .into_iter()
            .map(|root| {
                let last = store.last_indexed_at_for_root(&root).unwrap_or(None);
                (root, last)
            })
            .collect();
        Some((health, open_questions, roots))
    } else {
        None
    };

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
        let coverage = coverage.map(|(health, open_questions, roots)| CoverageJson {
            files: health.files,
            dirs: health.dirs,
            files_with_chunks: health.files_with_chunks,
            chunks: health.chunks,
            embedded_chunks: health.embedded_chunks,
            files_summarized: health.files_summarized,
            dirs_summarized: health.dirs_summarized,
            stale_summaries: health.stale_summaries,
            open_questions,
            roots: roots
                .into_iter()
                .map(|(path, last_indexed_at)| RootCoverageJson {
                    path,
                    last_indexed_at,
                })
                .collect(),
        });
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
            usage_week: (usage.calls > 0).then_some(UsageWeekJson {
                calls: usage.calls,
                served: usage.bytes_served,
                counterfactual: usage.bytes_counterfactual,
            }),
            coverage,
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

    // Measured token savings — same wording as MCP get_stats (one source of
    // truth in UsageSummary::savings_line; approximate by definition).
    if let Some(line) = usage.savings_line() {
        println!("Savings:  {line}");
    }

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

    if let Some((health, open_questions, roots)) = &coverage {
        println!();
        println!("Coverage:");
        if health.files == 0 && health.dirs == 0 {
            println!("  (index is empty — run `indexa index <path>` to build it)");
        } else {
            println!(
                "  Files:      {} files, {} dirs indexed",
                health.files, health.dirs
            );
            if health.chunks == 0 {
                println!(
                    "  Deep:       no chunks yet — run `indexa deep <path>` to make files searchable"
                );
            } else {
                println!(
                    "  Deep:       {}/{} files chunked ({}) — {} chunks",
                    health.files_with_chunks,
                    health.files,
                    pct(health.files_with_chunks, health.files),
                    health.chunks
                );
                println!(
                    "  Embedded:   {}/{} chunks ({})",
                    health.embedded_chunks,
                    health.chunks,
                    pct(health.embedded_chunks, health.chunks)
                );
                if health.embedded_chunks < health.chunks {
                    println!(
                        "              {} chunks have no embedding — dense search can't see them; re-run `indexa deep <path>`",
                        health.chunks - health.embedded_chunks
                    );
                }
            }
            if health.files_summarized == 0 && health.dirs_summarized == 0 {
                println!("  Summaries:  none yet — run `indexa summarize <path>`");
            } else {
                println!(
                    "  Summaries:  {}/{} files ({}), {}/{} dirs ({})",
                    health.files_summarized,
                    health.files,
                    pct(health.files_summarized, health.files),
                    health.dirs_summarized,
                    health.dirs,
                    pct(health.dirs_summarized, health.dirs)
                );
                if health.stale_summaries > 0 {
                    println!(
                        "  Stale:      {} summaries older than their file — re-run `indexa summarize <path>`",
                        health.stale_summaries
                    );
                } else {
                    println!("  Stale:      none — every summary is newer than its file");
                }
            }
            println!(
                "  Queue:      {} pending / {} in-flight / {} failed",
                queue.pending, queue.in_flight, queue.failed
            );
            if *open_questions > 0 {
                println!("  Questions:  {open_questions} open — `indexa review list`");
            } else {
                println!("  Questions:  0 open");
            }
        }
        if !roots.is_empty() {
            println!();
            println!("Last indexed per root:");
            for (root, last) in roots {
                match last {
                    Some(ts) => println!("  {:<20}  {root}", format_unix_timestamp(*ts)),
                    None => println!("  {:<20}  {root}", "never deep-indexed"),
                }
            }
        }
    }

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
