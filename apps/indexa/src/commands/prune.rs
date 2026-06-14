use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::require_index_db;

/// Resolved (dismissed/expired) review questions older than this are GC'd by
/// `indexa prune` — same horizon as `indexa review gc`'s default. GC of a
/// dismissed row also forgets its sticky dismissal, so past the horizon a
/// question may legitimately be asked again.
const DECISION_GC_SECS: i64 = 365 * 86_400;

/// `indexa prune` — garbage-collect orphaned index rows (chunks/summaries whose path has no
/// `entries` row) plus stale decision-ledger rows. Orphans accumulate when a root is removed
/// or re-pointed; the normal pipeline never revisits them. Index-only and recoverable by
/// re-scanning the affected paths (ledger GC is the one irreversible part — see above).
pub(crate) async fn cmd_prune(dry_run: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;

    let counts = store.count_orphans()?;
    // gc_decisions has no dry-run mode; the count-only twin keeps --dry-run honest.
    let gc_candidates = store.gc_decisions_count(DECISION_GC_SECS)?;
    if counts.is_empty() && gc_candidates == 0 {
        println!("Index is clean — no orphaned rows or stale review questions to prune.");
        return Ok(());
    }

    if dry_run {
        println!(
            "Would prune {} orphaned chunk(s), {} stale queue row(s), {} summary(ies), and {} \
classification(s) (no matching entry).",
            counts.chunks, counts.queue, counts.summaries, counts.classifications
        );
        println!(
            "Would GC {gc_candidates} resolved review question(s) (dismissed/expired > 365 days)."
        );
        println!("Run `indexa prune` (without --dry-run) to remove them.");
        return Ok(());
    }

    let removed = store.prune_orphans()?;
    let gcd = store.gc_decisions(DECISION_GC_SECS)?;
    println!(
        "Pruned {} orphaned chunk(s), {} stale queue row(s), {} summary(ies), and {} \
classification(s).",
        removed.chunks, removed.queue, removed.summaries, removed.classifications
    );
    if gcd > 0 {
        println!(
            "Review questions GC'd: {gcd} (forgotten dismissals may be asked again if their \
evidence still stands)"
        );
    } else {
        println!("Review questions GC'd: 0");
    }
    Ok(())
}
