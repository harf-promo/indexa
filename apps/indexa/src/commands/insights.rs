use anyhow::Result;
use indexa_core::store::Store;
use std::time::{SystemTime, UNIX_EPOCH};

use super::helpers::{format_size, require_index_db};

pub(crate) async fn cmd_insights_largest(limit: usize, json: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let rows = store.find_largest(limit)?;
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|e| serde_json::json!({ "path": e.path, "size": e.size }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No indexed files found.");
        return Ok(());
    }
    println!("Largest indexed files (top {}):", rows.len());
    println!("{:>10}  Path", "Size");
    println!("{}", "─".repeat(60));
    for e in &rows {
        println!("{:>10}  {}", format_size(e.size), e.path);
    }
    Ok(())
}

pub(crate) async fn cmd_insights_languages(json: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let rows = store.language_breakdown()?;
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|l| serde_json::json!({ "language": l.language, "chunks": l.chunks }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No language-tagged chunks yet. Run `indexa deep` on source files first.");
        return Ok(());
    }
    let total: u64 = rows.iter().map(|l| l.chunks).sum();
    println!("Language breakdown by indexed chunks ({total} total):");
    println!("{:>10}  {:>6}  Language", "Chunks", "Share");
    println!("{}", "─".repeat(40));
    for l in &rows {
        let pct = if total > 0 {
            l.chunks as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        println!("{:>10}  {:>5.1}%  {}", l.chunks, pct, l.language);
    }
    Ok(())
}

pub(crate) async fn cmd_insights_duplicates(threshold: f32, exact: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;

    if exact {
        let clusters = store.find_exact_duplicates()?;
        if clusters.is_empty() {
            println!("No exact duplicates found.");
            return Ok(());
        }
        println!("Exact duplicate clusters ({}):", clusters.len());
        for (i, cluster) in clusters.iter().enumerate() {
            println!("\n  Cluster {} ({} files):", i + 1, cluster.paths.len());
            for p in &cluster.paths {
                println!("    {p}");
            }
        }
    } else {
        println!("Scanning for near-duplicates (threshold={threshold:.2})…");
        let clusters = store.find_near_duplicates(threshold)?;
        if clusters.is_empty() {
            println!("No near-duplicates found at threshold {threshold:.2}.");
            println!("Tip: try a lower threshold, e.g. --threshold 0.85");
            return Ok(());
        }
        println!("Near-duplicate clusters ({}):", clusters.len());
        for (i, cluster) in clusters.iter().enumerate() {
            println!(
                "\n  Cluster {} ({} files, similarity {:.2}):",
                i + 1,
                cluster.paths.len(),
                cluster.similarity
            );
            for p in &cluster.paths {
                println!("    {p}");
            }
        }
    }
    Ok(())
}

pub(crate) async fn cmd_insights_stale(days: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let stale = store.find_stale_entries(days)?;
    if stale.is_empty() {
        println!("No stale directories found (threshold: {days} days).");
        return Ok(());
    }
    println!(
        "Directories not modified in the last {days} days ({}):",
        stale.len()
    );
    println!("{:>8}  Path", "Days ago");
    println!("{}", "─".repeat(60));
    for s in &stale {
        println!("{:>8}  {}", s.days_since_modified, s.path);
    }
    Ok(())
}

pub(crate) async fn cmd_insights_diff(days: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - days * 86_400)
        .unwrap_or(0);
    let diff = store.weekly_diff(since)?;
    println!("Index changes in the last {days} day(s):");
    println!();
    if diff.added.is_empty() {
        println!("  Added: none");
    } else {
        println!("  Added ({}):", diff.added_count);
        for p in diff.added.iter().take(50) {
            println!("    + {p}");
        }
        if diff.added_count > 50 {
            println!("    … and {} more", diff.added_count - 50);
        }
    }
    println!();
    if diff.modified.is_empty() {
        println!("  Modified: none");
    } else {
        println!("  Modified ({}):", diff.modified_count);
        for p in diff.modified.iter().take(50) {
            println!("    ~ {p}");
        }
        if diff.modified_count > 50 {
            println!("    … and {} more", diff.modified_count - 50);
        }
    }
    Ok(())
}
