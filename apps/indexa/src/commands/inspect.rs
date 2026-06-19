use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::{format_size, format_unix_timestamp, require_index_db};

/// `indexa inspect <path>` — a plain-text "what's indexed here" view: the scan entry, indexed
/// chunks, summary presence, classification, resolved weight, and code-graph relationships for a
/// single path. The index is a derived cache over your real files; this makes its contents legible
/// (answering "is it a black box?") — everything shown is re-derivable by re-indexing.
pub(crate) async fn cmd_inspect(path: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let expanded = shellexpand::tilde(&path).into_owned();
    let store = Store::open(&db_path)?;

    let entry = store.entry_by_path(&expanded)?;
    let summary = store.summary_by_path(&expanded)?;
    let chunks = store.chunks_for_path(&expanded, 0)?;

    if entry.is_none() && summary.is_none() && chunks.is_empty() {
        println!("Nothing indexed at {expanded}.");
        println!("Index it with: indexa index \"{expanded}\"");
        return Ok(());
    }

    let icon = match (&entry, &summary) {
        (Some(e), _) if e.kind == "dir" => "📁",
        (_, Some(s)) if s.kind == "dir" => "📁",
        _ => "📄",
    };
    println!("{icon} {expanded}");

    // Entry facts (kind / size / mtime).
    if let Some(e) = &entry {
        let when = e
            .modified_s
            .map(format_unix_timestamp)
            .unwrap_or_else(|| "unknown".to_owned());
        println!(
            "  Entry:     {} · {} · modified {}",
            e.kind,
            format_size(e.size),
            when
        );
    } else {
        println!("  Entry:     (no scan entry — summary/chunks only)");
    }

    // Indexed chunks (the searchable pieces).
    if chunks.is_empty() {
        println!("  Chunks:    none — run `indexa deep \"{expanded}\"` to parse + embed");
    } else {
        let lang = chunks.iter().find_map(|c| c.language.clone());
        let lang_note = lang.map(|l| format!(" ({l})")).unwrap_or_default();
        println!("  Chunks:    {}{lang_note}", chunks.len());
        for c in chunks.iter().take(5) {
            let h: &str = if c.heading.trim().is_empty() {
                "(no heading)"
            } else {
                c.heading.as_str()
            };
            println!("               · {h}");
        }
        if chunks.len() > 5 {
            println!("               … {} more", chunks.len() - 5);
        }
    }

    // Summary presence.
    match &summary {
        Some(rec) => {
            if let Some(a) = &rec.summary_l0 {
                println!("  Abstract:  {a}");
            }
            println!("  Summary:   present (model {})", rec.model);
        }
        None => println!("  Summary:   none — run `indexa summarize \"{expanded}\"`"),
    }

    // Classification.
    if let Some(cls) = store.classification_for(&expanded)? {
        println!(
            "  Category:  {} ({:.0}% · {})",
            cls.category,
            cls.confidence * 100.0,
            cls.source
        );
    }

    // Detected application/structure (v0.66): what kind of thing this directory is.
    let apps = store.apps_for_dir(&expanded).unwrap_or_default();
    if let Some(primary) = apps.iter().find(|a| a.is_primary).or_else(|| apps.first()) {
        let others: Vec<&str> = apps
            .iter()
            .filter(|a| a.app_kind != primary.app_kind)
            .map(|a| a.app_name.as_str())
            .collect();
        let also = if others.is_empty() {
            String::new()
        } else {
            format!(" (also: {})", others.join(", "))
        };
        println!(
            "  App:       {} [{}]{also}",
            primary.app_name, primary.family
        );
    }

    // Resolved importance weight (only show when non-neutral).
    let w = store.weight_for(&expanded)?;
    if (w - 1.0).abs() > f32::EPSILON {
        println!("  Weight:    {w:.2} (1.0 = neutral)");
    }

    // Code-graph relationships (outgoing edges).
    let edges = store.edges_from(&expanded).unwrap_or_default();
    if !edges.is_empty() {
        let count = |k: &str| edges.iter().filter(|e| e.kind == k).count();
        println!(
            "  Graph:     {} imports · {} defines · {} calls (bare-name matched)",
            count("imports"),
            count("defines"),
            count("calls"),
        );
    }

    println!();
    println!(
        "The index is a derived cache over your real files — every field above is re-derivable by \
         re-indexing, and your source files are never modified."
    );
    Ok(())
}
