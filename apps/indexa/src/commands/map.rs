use anyhow::Result;
use indexa_core::store::Store;
use std::io::IsTerminal;

use super::helpers::{format_size, require_index_db};

pub(crate) async fn cmd_map(depth: usize) -> Result<()> {
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
