use anyhow::Result;
use indexa_core::{
    store::Store,
    walker::{walk, WalkConfig},
};

use super::helpers::{index_db_path, resolve_roots};

pub(crate) async fn cmd_scan(paths: Vec<String>, all: bool) -> Result<()> {
    let roots = resolve_roots(paths, all)?;
    let db_path = index_db_path()?;
    let mut store = Store::open(&db_path)?;

    for root in &roots {
        println!("Scanning {}", root.display());
        let entries = walk(root, &WalkConfig::default())?;
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
    }

    println!("\nIndex saved to {}", db_path.display());
    println!("Run `indexa map` to see a summary.");
    println!("Run `indexa deep <path>` to parse and embed file contents.");
    Ok(())
}
