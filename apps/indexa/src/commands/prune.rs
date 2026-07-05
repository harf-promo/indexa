use anyhow::Result;
use indexa_core::config;
use indexa_core::decisions::detectors;
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
pub(crate) async fn cmd_prune(dry_run: bool, vacuum: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;

    let counts = store.count_orphans()?;
    // gc_decisions has no dry-run mode; the count-only twin keeps --dry-run honest.
    let gc_candidates = store.gc_decisions_count(DECISION_GC_SECS)?;
    // v0.39: low-value review questions the new noise filters reject (idiom / disabled
    // symbol-ambiguity, asset/generated duplicate clusters). Respects the user's actual
    // config so an opted-in symbol_ambiguity isn't wrongly dismissed.
    let review_cfg = config::load_default().map(|c| c.review).unwrap_or_default();
    let noise_candidates = detectors::sweep_filtered_noise(&mut store, &review_cfg, true)?;
    if counts.is_empty() && gc_candidates == 0 && noise_candidates == 0 {
        println!("Index is clean — no orphaned rows or stale review questions to prune.");
        // Still allow --vacuum on a clean-but-bloated DB (e.g. after bulk deletes).
        if !vacuum {
            return Ok(());
        }
    }

    if dry_run {
        println!(
            "Would prune {} orphaned chunk(s), {} stale queue row(s), {} summary(ies), {} \
classification(s), and {} app detection(s) (no matching entry).",
            counts.chunks,
            counts.queue,
            counts.summaries,
            counts.classifications,
            counts.directory_apps
        );
        println!(
            "Would GC {gc_candidates} resolved review question(s) (dismissed/expired > 365 days)."
        );
        if noise_candidates > 0 {
            println!(
                "Would dismiss {noise_candidates} low-value review question(s) (idiom / disabled \
symbol-ambiguity, asset/generated duplicates)."
            );
        }
        println!("Run `indexa prune` (without --dry-run) to remove them.");
        return Ok(());
    }

    let removed = store.prune_orphans()?;
    let gcd = store.gc_decisions(DECISION_GC_SECS)?;
    let dismissed = detectors::sweep_filtered_noise(&mut store, &review_cfg, false)?;
    println!(
        "Pruned {} orphaned chunk(s), {} stale queue row(s), {} summary(ies), {} \
classification(s), and {} app detection(s).",
        removed.chunks,
        removed.queue,
        removed.summaries,
        removed.classifications,
        removed.directory_apps
    );
    if dismissed > 0 {
        println!("Dismissed {dismissed} low-value review question(s) (idiom/asset noise).");
    }
    if gcd > 0 {
        println!(
            "Review questions GC'd: {gcd} (forgotten dismissals may be asked again if their \
evidence still stands)"
        );
    } else {
        println!("Review questions GC'd: 0");
    }

    if vacuum {
        let before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        match store.vacuum() {
            Ok(()) => {
                let after = std::fs::metadata(&db_path)
                    .map(|m| m.len())
                    .unwrap_or(before);
                let reclaimed = before.saturating_sub(after);
                println!(
                    "VACUUM complete: {} → {} ({} reclaimed).",
                    super::helpers::format_size(before),
                    super::helpers::format_size(after),
                    super::helpers::format_size(reclaimed),
                );
            }
            Err(e) => eprintln!("VACUUM failed (index left intact): {e:#}"),
        }
    }

    Ok(())
}
