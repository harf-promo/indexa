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

    let remove = |store: &mut Store, path: &str| {
        if recursive {
            store.delete_subtree(path)
        } else {
            store.delete_entry(path)
        }
    };
    let mut total_removed = 0usize;
    for expanded in &expanded {
        // Scanned roots are stored canonicalized (`resolve_roots`), so `rm ./src` or a path
        // through a symlink matched nothing. Try the path as given first — it is exactly what
        // the user named and confirmed — and only when it removes nothing retry its canonical
        // form, the same way `review history` looks a subject up.
        let mut path = expanded.clone();
        let mut n = remove(store, &path)?;
        if n == 0 {
            let canonical = super::helpers::resolve_roots(vec![expanded.clone()], false)?
                .into_iter()
                .next()
                .map(|p| p.to_string_lossy().into_owned());
            if let Some(canonical) = canonical.filter(|c| c != expanded) {
                n = remove(store, &canonical)?;
                if n > 0 {
                    path = canonical;
                }
            }
        }
        total_removed += n;
        if recursive {
            println!("Removed subtree: {path} ({n} entries)");
        } else if n > 0 {
            println!("Removed: {path}");
        } else {
            println!("Not found in index: {path}");
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

    /// F23: `rm` used the path exactly as typed, but scanned roots are stored canonicalized,
    /// so a relative or `..`/symlinked spelling of an indexed path matched nothing. A path
    /// that misses literally falls back to its canonical form.
    #[test]
    fn rm_falls_back_to_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("a.rs"), "a").unwrap();
        std::fs::write(real.join("sub").join("b.rs"), "b").unwrap();
        let canon =
            super::super::helpers::resolve_roots(vec![real.to_string_lossy().into_owned()], false)
                .unwrap()
                .remove(0);
        let a = canon.join("a.rs").to_string_lossy().into_owned();
        let b = canon
            .join("sub")
            .join("b.rs")
            .to_string_lossy()
            .into_owned();
        let mut store = store_with(&[&a, &b]);

        // A non-canonical spelling of the same directory: real/sub/.. == real.
        let roundabout = real.join("sub").join("..");
        let file = roundabout.join("a.rs").to_string_lossy().into_owned();
        let n = rm_paths(&mut store, &[file], false, false, false).unwrap();
        assert_eq!(n, Some(1), "plain rm must find the canonical entry");

        let subtree = roundabout.join("sub").to_string_lossy().into_owned();
        let n = rm_paths(&mut store, &[subtree], true, true, false).unwrap();
        assert_eq!(n, Some(1), "rm -r must find the canonical subtree");
        assert_eq!(entry_count(&store), 0);
    }

    /// The literal path always wins: when it matches, the canonical form is not also
    /// removed, so `rm` never deletes more than the entry the user named.
    #[test]
    fn rm_prefers_the_literal_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("a.rs"), "a").unwrap();
        let canon =
            super::super::helpers::resolve_roots(vec![real.to_string_lossy().into_owned()], false)
                .unwrap()
                .remove(0);
        let literal = real
            .join("sub")
            .join("..")
            .join("a.rs")
            .to_string_lossy()
            .into_owned();
        let canonical = canon.join("a.rs").to_string_lossy().into_owned();
        assert_ne!(literal, canonical);
        let mut store = store_with(&[&literal, &canonical]);

        let n = rm_paths(
            &mut store,
            std::slice::from_ref(&literal),
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(n, Some(1));
        let left: String = store
            .db_connection()
            .query_row("SELECT path FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, canonical, "the canonical entry must survive");
    }
}
