use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::index_db_path;

pub(crate) async fn cmd_rm(paths: Vec<String>, recursive: bool) -> Result<()> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found.");
        return Ok(());
    }

    let mut store = Store::open(&db_path)?;
    let mut total_removed = 0usize;

    for path_str in &paths {
        let expanded = shellexpand::tilde(path_str).into_owned();
        if recursive {
            let n = store.delete_subtree(&expanded)?;
            total_removed += n;
            println!("Removed subtree: {expanded} ({n} entries)");
        } else {
            let n = store.delete_entry(&expanded)?;
            total_removed += n;
            if n > 0 {
                println!("Removed: {expanded}");
            } else {
                println!("Not found in index: {expanded}");
            }
        }
    }

    println!("Total removed: {total_removed} entries");
    Ok(())
}
