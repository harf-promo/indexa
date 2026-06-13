use anyhow::Result;
use indexa_core::{
    config::Config,
    store::Store,
    walker::{walk, WalkConfig},
};

use super::helpers::{index_db_path, resolve_roots};

pub(crate) async fn cmd_scan(paths: Vec<String>, all: bool, cfg: &Config) -> Result<()> {
    let roots = resolve_roots(paths, all)?;
    let db_path = index_db_path()?;
    let mut store = Store::open(&db_path)?;
    let walk_cfg = WalkConfig {
        respect_gitignore: cfg.scan.respect_gitignore,
        ignore: cfg.scan.ignore.clone(),
        ..Default::default()
    };

    for root in &roots {
        println!("Scanning {}", root.display());
        let entries = walk(root, &walk_cfg)?;
        let live_paths: std::collections::HashSet<String> = entries
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();

        store.upsert_entries(&entries)?;

        // Ghost-row cleanup: remove entries that were in the index but no longer on disk.
        let root_str = root.to_string_lossy().into_owned();
        let removed = store.reconcile_entries(&root_str, &live_paths)?;
        let count = live_paths.len();
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
