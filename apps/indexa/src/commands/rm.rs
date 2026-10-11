use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::index_db_path;

pub(crate) async fn cmd_rm(paths: Vec<String>, recursive: bool, yes: bool) -> Result<()> {
    use std::io::IsTerminal as _;

    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found.");
        return Ok(());
    }

    let mut store = Store::open(&db_path)?;
    rm_paths(
        &mut store,
        &paths,
        recursive,
        yes,
        std::io::stdin().is_terminal(),
    )?;
    Ok(())
}

/// [`cmd_rm`] with the store and terminal check injected, for tests. Returns the number of
/// entries removed, or `None` when the user declined the `-r` prompt.
///
/// `-r` drops every entry, chunk and summary under each path in one go, so it confirms first
/// through [`super::helpers::confirm_or_bail_with`]: `--yes` proceeds, a TTY prompts, and a
/// non-interactive session without `--yes` refuses before anything is deleted. A plain `rm`
/// removes exactly the named entries and does not prompt.
fn rm_paths(
    store: &mut Store,
    paths: &[String],
    recursive: bool,
    yes: bool,
    is_tty: bool,
) -> Result<Option<usize>> {
    let expanded: Vec<String> = paths
        .iter()
        .map(|p| shellexpand::tilde(p).into_owned())
        .collect();

    if recursive {
        let confirmed = super::helpers::confirm_or_bail_with(
            &format!(
                "Remove {} and everything indexed under it from the index? (Files on disk are \
                 not touched.)",
                expanded.join(", ")
            ),
            false,
            yes,
            is_tty,
            &format!(
                "Refusing to remove {} subtree(s) from the index in a non-interactive session. \
                 This drops every entry, chunk and summary under each path; re-run with --yes \
                 to confirm.",
                expanded.len()
            ),
        )?;
        if !confirmed {
            println!("Aborted.");
            return Ok(None);
        }
    }

    let mut total_removed = 0usize;
    for expanded in &expanded {
        if recursive {
            let n = store.delete_subtree(expanded)?;
            total_removed += n;
            println!("Removed subtree: {expanded} ({n} entries)");
        } else {
            let n = store.delete_entry(expanded)?;
            total_removed += n;
            if n > 0 {
                println!("Removed: {expanded}");
            } else {
                println!("Not found in index: {expanded}");
            }
        }
    }

    println!("Total removed: {total_removed} entries");
    Ok(Some(total_removed))
}

#[cfg(test)]
mod tests {
    use super::rm_paths;
    use indexa_core::store::Store;
    use indexa_core::walker::{Entry, EntryKind};

    fn store_with(paths: &[&str]) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let entries: Vec<Entry> = paths
            .iter()
            .map(|p| Entry {
                path: (*p).into(),
                kind: EntryKind::File,
                size: 1,
                modified: None,
                hint: None,
                is_binary: false,
            })
            .collect();
        store.upsert_entries(&entries).unwrap();
        store
    }

    fn entry_count(store: &Store) -> i64 {
        store
            .db_connection()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap()
    }

    /// F23: `rm -r` deleted whole subtrees with no confirmation of any kind. A
    /// non-interactive session without `--yes` must refuse before touching the index.
    #[test]
    fn recursive_rm_refuses_without_yes_when_not_a_tty() {
        let mut store = store_with(&["/proj/a.rs", "/proj/sub/b.rs"]);
        let err = rm_paths(&mut store, &["/proj".into()], true, false, false).unwrap_err();
        assert!(err.to_string().contains("--yes"), "got: {err}");
        assert_eq!(entry_count(&store), 2, "nothing may be deleted on refusal");
    }

    #[test]
    fn recursive_rm_with_yes_removes_the_subtree() {
        let mut store = store_with(&["/proj/a.rs", "/proj/sub/b.rs", "/other/c.rs"]);
        let n = rm_paths(&mut store, &["/proj".into()], true, true, false).unwrap();
        assert_eq!(n, Some(2));
        assert_eq!(
            entry_count(&store),
            1,
            "entries outside the subtree survive"
        );
    }

    /// A plain (non-recursive) `rm` removes only the named entry and keeps working in
    /// scripts without `--yes`.
    #[test]
    fn plain_rm_needs_no_confirmation() {
        let mut store = store_with(&["/proj/a.rs", "/proj/b.rs"]);
        let n = rm_paths(&mut store, &["/proj/a.rs".into()], false, false, false).unwrap();
        assert_eq!(n, Some(1));
        assert_eq!(entry_count(&store), 1);
    }
}
