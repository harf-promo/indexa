use std::path::PathBuf;

use anyhow::Result;
use indexa_core::{
    config::Config,
    store::Store,
    walker::{ensure_walked_root_present, walk_streaming, WalkConfig},
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
        custom_ignore: cfg.scan.custom_ignore,
        ..Default::default()
    };

    let skipped = scan_roots(&mut store, &roots, &walk_cfg)?;

    println!("\nIndex saved to {}", db_path.display());
    println!("Run `indexa map` to see a summary.");
    println!("Run `indexa deep <path>` to parse and embed file contents.");
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(|r| r.display().to_string()).collect();
        anyhow::bail!(
            "{} root(s) skipped because they are missing or unreadable (index left untouched): {}",
            skipped.len(),
            list.join(", ")
        );
    }
    Ok(())
}

/// Scan each root into `store` and prune the rows the walk didn't re-see. Returns the roots that
/// were skipped because they were missing or unreadable — their existing index rows are left
/// untouched, so an unmounted drive or a transient permission error never wipes a root's index.
fn scan_roots(store: &mut Store, roots: &[PathBuf], walk_cfg: &WalkConfig) -> Result<Vec<PathBuf>> {
    // One generation per `indexa scan` run, stamped on every upserted row so the post-scan prune
    // (reconcile_by_generation) can drop rows this run didn't re-see — removed from disk, or stale
    // from an interrupted prior scan — without ever holding a full live-path set in memory.
    let generation = store.next_scan_generation()?;
    let mut skipped = Vec::new();
    for root in roots {
        println!("Scanning {}", root.display());
        // Stream the walk so a whole-computer scan stays bounded-memory: upsert each batch
        // (stamped with this run's generation) as it arrives, instead of collecting every entry
        // into one Vec before writing anything.
        let mut count = 0usize;
        walk_streaming(root, walk_cfg, |batch| {
            count += batch.len();
            store.upsert_entries_with_generation(&batch, Some(generation))
        })?;

        // The walk is fail-open, so a missing/unreadable root yields nothing — and pruning on
        // that would delete everything indexed under it. Skip the prune for this root instead.
        if let Err(e) = ensure_walked_root_present(root, count) {
            eprintln!("  skipped: {e:#}");
            eprintln!(
                "  its existing index was left untouched — remount it or fix permissions and \
                 re-run `indexa scan`, or `indexa rm -r {}` to drop it",
                root.display()
            );
            skipped.push(root.clone());
            continue;
        }

        // Ghost-row cleanup: prune entries this scan did NOT re-stamp (removed from disk, or a
        // stale generation left by an interrupted prior scan) — a cheap SQL sweep, no live-path
        // set held.
        let root_str = root.to_string_lossy().into_owned();
        let removed = store.reconcile_by_generation(&root_str, generation)?;
        if removed > 0 {
            println!("  {count} entries, removed {removed} ghost rows");
        } else {
            println!("  {count} entries");
        }
        // Self-heal: drop chunks/summaries left orphaned (no entry row) — e.g. build
        // artifacts indexed by an older version, or rows stranded by a partial delete.
        // `reconcile_by_generation` only cleans *ghost entries*, never orphans with no entry.
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
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries_under(store: &Store, root: &std::path::Path) -> i64 {
        store
            .db_connection()
            .query_row(
                // Prefix match without LIKE: tempdir names can hold `_`, and Windows uses `\\`.
                "SELECT COUNT(*) FROM entries WHERE substr(path, 1, length(?1)) = ?1",
                [root.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn missing_root_is_skipped_and_keeps_its_index_while_others_still_prune() {
        // Regression: a bare `indexa scan` while a stored root's drive was unmounted reported
        // "0 entries, removed N ghost rows" and wiped the root's whole index, exiting 0.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let drive = base.join("drive");
        let home = base.join("home");
        for root in [&drive, &home] {
            std::fs::create_dir(root).unwrap();
            std::fs::write(root.join("a.md"), "# a").unwrap();
            std::fs::write(root.join("b.md"), "# b").unwrap();
        }
        let cfg = WalkConfig::default();
        let roots = vec![drive.clone(), home.clone()];
        let mut store = Store::open_in_memory().unwrap();

        assert!(scan_roots(&mut store, &roots, &cfg).unwrap().is_empty());
        let drive_rows = entries_under(&store, &drive);
        assert_eq!(drive_rows, 3, "root dir + 2 files");

        // Unmount the drive; delete a file from the other root.
        std::fs::rename(&drive, base.join("drive.unmounted")).unwrap();
        std::fs::remove_file(home.join("b.md")).unwrap();

        let skipped = scan_roots(&mut store, &roots, &cfg).unwrap();
        assert_eq!(skipped, vec![drive.clone()]);
        assert_eq!(
            entries_under(&store, &drive),
            drive_rows,
            "the missing root's index must survive"
        );
        assert_eq!(
            entries_under(&store, &home),
            2,
            "a present root still prunes its own ghosts"
        );
    }
}
