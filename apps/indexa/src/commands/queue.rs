use anyhow::{bail, Result};
use indexa_core::store::Store;

pub(crate) fn cmd_queue_recover_stuck(apply: bool) -> Result<()> {
    let Some(db_path) = super::helpers::require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let paths = store.unowned_in_flight()?;
    if paths.is_empty() {
        println!("No ownerless in-flight queue items.");
        return Ok(());
    }
    println!("{} ownerless in-flight item(s):", paths.len());
    for path in &paths {
        println!("  {path}");
    }
    if !apply {
        println!("Dry run only. Re-run with --apply to recover these rows.");
        return Ok(());
    }
    let (requeued, failed) = store.recover_unowned_in_flight(3)?;
    println!("Recovered: {requeued} requeued, {failed} marked failed at the attempt cap.");
    if requeued + failed != paths.len() {
        bail!(
            "queue changed during recovery; listed {} but updated {}",
            paths.len(),
            requeued + failed
        );
    }
    Ok(())
}
