use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::require_index_db;

/// `indexa prune` — garbage-collect orphaned index rows (chunks/summaries whose path has no
/// `entries` row). These accumulate when a root is removed or re-pointed; the normal pipeline
/// never revisits them. Index-only and recoverable by re-scanning the affected paths.
pub(crate) async fn cmd_prune(dry_run: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;

    let counts = store.count_orphans()?;
    if counts.is_empty() {
        println!("Index is clean — no orphaned rows to prune.");
        return Ok(());
    }

    if dry_run {
        println!(
            "Would prune {} orphaned chunk(s) and {} orphaned summary(ies) (no matching entry).",
            counts.chunks, counts.summaries
        );
        println!("Run `indexa prune` (without --dry-run) to remove them.");
        return Ok(());
    }

    let removed = store.prune_orphans()?;
    println!(
        "Pruned {} orphaned chunk(s) and {} orphaned summary(ies).",
        removed.chunks, removed.summaries
    );
    Ok(())
}
