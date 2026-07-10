use anyhow::Result;
use indexa_core::{
    config::Config,
    store::Store,
    walker::{walk_streaming, WalkConfig},
};

use super::helpers::{check_huge_root_guard, index_db_path, resolve_target_roots};

pub(crate) async fn cmd_scan(paths: Vec<String>, all: bool, yes: bool, cfg: &Config) -> Result<()> {
    let roots = resolve_target_roots(paths, all)?;
    if !yes {
        for root in &roots {
            check_huge_root_guard(root)?;
        }
    }
    let db_path = index_db_path()?;
    let mut store = Store::open(&db_path)?;
    let walk_cfg = WalkConfig {
        respect_gitignore: cfg.scan.respect_gitignore,
        ignore: cfg.scan.ignore.clone(),
        include_sensitive: cfg.scan.include_sensitive,
        threads: cfg.scan.threads,
        ..Default::default()
    };

    // One generation per scan run, stamped on every upserted row so the post-scan prune can drop
    // rows this run didn't touch (removed from disk, or stale from an interrupted prior scan).
    let generation = store.next_scan_generation()?;
    for root in &roots {
        println!("Scanning {}", root.display());
        let root_str = root.to_string_lossy().into_owned();
        // Stream the walk so a whole-computer scan stays bounded-memory: upsert each batch (stamped
        // with this run's generation) as it arrives instead of collecting every entry up front.
        let mut count = 0usize;
        walk_streaming(root, &walk_cfg, |batch| {
            count += batch.len();
            store.upsert_entries_with_generation(&batch, Some(generation))
        })?;

        // Ghost-row cleanup: prune entries this scan did NOT re-stamp (removed from disk, or a
        // stale generation left by an interrupted prior scan) — no live-path set held.
        let removed = store.reconcile_by_generation(&root_str, generation)?;
        if removed > 0 {
            println!("  {count} entries, removed {removed} ghost rows");
        } else {
            println!("  {count} entries");
        }
        // Self-heal: drop chunks/summaries left orphaned (no entry row) — e.g. build
        // artifacts indexed by an older version, or rows stranded by a partial delete.
        // `reconcile_entries` only cleans *ghost entries*, never orphans with no entry.
        let orphans = store.prune_orphans()?;
        if !orphans.is_empty() {
            println!(
                "  pruned {} orphaned chunk(s){}",
                orphans.chunks,
                if orphans.summaries > 0 {
                    format!(" and {} summary(ies)", orphans.summaries)
                } else {
                    String::new()
                }
            );
        }
    }

    println!("\nIndex saved to {}", db_path.display());
    println!("Run `indexa map` to see a summary.");
    println!("Run `indexa deep <path>` to parse and embed file contents.");
    Ok(())
}
