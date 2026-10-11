use anyhow::Result;
use indexa_core::store::Store;
use std::io::Write;

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
        &mut std::io::stdout(),
    )?;
    Ok(())
}

/// What one `rm` argument removes, decided before anything is deleted so the confirmation
/// names the path that will actually go.
struct Target {
    /// The argument as typed (after `~` expansion).
    typed: String,
    /// Its canonical form, when that differs from `typed` and was looked up.
    canonical: Option<String>,
    /// `typed` matched nothing, so the removal uses `canonical`.
    via_canonical: bool,
    /// The last component of `typed` is a symlink.
    final_symlink: bool,
    /// Entries the removal will delete; 0 means nothing matched.
    count: usize,
}

impl Target {
    fn path(&self) -> &str {
        match &self.canonical {
            Some(canonical) if self.via_canonical => canonical,
            _ => &self.typed,
        }
    }

    /// `"/proj/src (3 entries)"`: the path that will be removed and how much goes with it.
    fn describe(&self) -> String {
        format!("{} ({})", self.path(), entries(self.count))
    }

    /// `"./src resolves to /proj/src"` when the removal uses the canonical form.
    fn resolution(&self) -> Option<String> {
        self.via_canonical
            .then(|| format!("{} resolves to {}", self.typed, self.path()))
    }
}

fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

fn entries(n: usize) -> String {
    if n == 1 {
        "1 entry".to_owned()
    } else {
        format!("{n} entries")
    }
}

/// [`cmd_rm`] with the store, terminal check and output injected, for tests. Returns the
/// number of entries removed, or `None` when the user declined the prompt.
///
/// Every argument is resolved before anything is deleted: the path as typed wins when it
/// matches anything, otherwise its canonical form (scanned roots are stored canonicalized, so
/// `rm ./src` or a path through a symlinked directory would match nothing as typed).
///
/// `-r` drops every entry, chunk and summary under each path in one go, so it confirms first
/// through [`super::helpers::confirm_or_bail_with`], naming the path that will be removed and
/// its entry count: `--yes` proceeds, a TTY prompts, and a non-interactive session without
/// `--yes` refuses before anything is deleted. A `-r` path that is a filesystem root or empty
/// matches every entry, so it is refused outright unless `--yes` is given, even on a TTY.
///
/// A plain `rm` removes exactly the named entries and does not prompt, with one exception:
/// like `rm` on disk, it does not follow a symlink in the last component of the path. When the
/// link is not itself indexed but its target is, removing the target needs the same
/// confirmation as `-r`.
fn rm_paths(
    store: &mut Store,
    paths: &[String],
    recursive: bool,
    yes: bool,
    is_tty: bool,
    out: &mut dyn Write,
) -> Result<Option<usize>> {
    let targets = paths
        .iter()
        .map(|p| resolve_target(store, &shellexpand::tilde(p), recursive))
        .collect::<Result<Vec<_>>>()?;

    if recursive && !yes {
        if let Some(t) = targets
            .iter()
            .find(|t| is_whole_index(&t.typed) || is_whole_index(t.path()))
        {
            let shown = if t.via_canonical {
                format!("{} (resolves to {})", t.typed, t.path())
            } else {
                format!("{:?}", t.typed)
            };
            anyhow::bail!(
                "Refusing to remove {shown} recursively: a filesystem root or an empty path \
                 matches every entry in the index. Re-run with --yes if you really mean to \
                 empty the index."
            );
        }
    }

    let to_confirm: Vec<&Target> = targets
        .iter()
        .filter(|t| t.count > 0 && (recursive || (t.via_canonical && t.final_symlink)))
        .collect();
    if !to_confirm.is_empty() {
        let list = join(to_confirm.iter().map(|t| t.describe()));
        let resolved = join(to_confirm.iter().filter_map(|t| t.resolution()));
        let (prompt, refusal) = if recursive {
            let (note, refusal_note) = if resolved.is_empty() {
                (String::new(), String::new())
            } else {
                (format!(" {resolved}."), format!("; {resolved}"))
            };
            (
                format!(
                    "Remove {list} and everything indexed under it from the index?{note} (Files \
                     on disk are not touched.)"
                ),
                format!(
                    "Refusing to remove {list} from the index in a non-interactive \
                     session{refusal_note}. This drops every entry, chunk and summary under each \
                     path; re-run with --yes to confirm."
                ),
            )
        } else {
            (
                format!(
                    "{resolved}: the symlink is not indexed, but its target is. Remove {list} \
                     from the index? (Files on disk are not touched.)"
                ),
                format!(
                    "Refusing to follow a symlink in a non-interactive session: {resolved}. A \
                     plain rm removes a symlink's target only when confirmed; name the target \
                     directly, or re-run with --yes to remove {list}."
                ),
            )
        };
        if !super::helpers::confirm_or_bail_with(&prompt, false, yes, is_tty, &refusal)? {
            writeln!(out, "Aborted.")?;
            return Ok(None);
        }
    }

    let mut total_removed = 0usize;
    for t in &targets {
        let n = match t.count {
            0 => 0,
            _ if recursive => store.delete_subtree(t.path())?,
            _ => store.delete_entry(t.path())?,
        };
        total_removed += n;
        let shown = if t.via_canonical {
            format!("{} (resolved from {})", t.path(), t.typed)
        } else {
            t.typed.clone()
        };
        if n == 0 {
            match &t.canonical {
                Some(canonical) if !t.via_canonical => {
                    writeln!(out, "Not found in index: {} (or {canonical})", t.typed)?;
                }
                _ => writeln!(out, "Not found in index: {shown}")?,
            }
        } else if recursive {
            writeln!(out, "Removed subtree: {shown} ({})", entries(n))?;
        } else {
            writeln!(out, "Removed: {shown}")?;
        }
    }

    writeln!(out, "Total removed: {total_removed} entries")?;
    Ok(Some(total_removed))
}

/// Decide what `typed` removes without deleting anything: the path as typed when it matches
/// anything, else its canonical form (the same way `review history` looks a subject up).
fn resolve_target(store: &Store, typed: &str, recursive: bool) -> Result<Target> {
    let count = |path: &str| -> Result<usize> {
        if recursive {
            subtree_entry_count(store, path)
        } else {
            Ok(usize::from(store.entry_exists(path)?))
        }
    };
    let mut target = Target {
        typed: typed.to_owned(),
        canonical: None,
        via_canonical: false,
        final_symlink: false,
        count: count(typed)?,
    };
    if target.count > 0 {
        return Ok(target);
    }
    target.canonical = super::helpers::resolve_roots(vec![typed.to_owned()], false)?
        .into_iter()
        .next()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|c| c != typed);
    if let Some(canonical) = &target.canonical {
        target.count = count(canonical)?;
        target.via_canonical = target.count > 0;
        let last = typed.trim_end_matches(['/', '\\']);
        target.final_symlink = !last.is_empty()
            && std::fs::symlink_metadata(last).is_ok_and(|m| m.file_type().is_symlink());
    }
    Ok(target)
}

/// Entries [`Store::delete_subtree`] would delete for `prefix`: the exact path plus everything
/// under `prefix<sep>`, with LIKE wildcards escaped — the same match as the store's private
/// `subtree_match`, so `/a/b` never counts `/a/bc`.
fn subtree_entry_count(store: &Store, prefix: &str) -> Result<usize> {
    let sep = prefix
        .rfind(['/', '\\'])
        .map_or('/', |i| prefix.as_bytes()[i] as char);
    let exact = prefix.trim_end_matches(['/', '\\']);
    let child = format!(
        "{}%",
        format!("{exact}{sep}")
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let n: i64 = store.db_connection().query_row(
        "SELECT COUNT(*) FROM entries
          WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'
             OR parent_path = ?1 OR parent_path LIKE ?2 ESCAPE '\\'",
        [exact, child.as_str()],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// An empty or whitespace path, or a filesystem root (`/`, `C:\`): as a subtree prefix it
/// matches every entry in the index.
fn is_whole_index(path: &str) -> bool {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let bytes = trimmed.as_bytes();
    trimmed.is_empty() || (bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

#[cfg(test)]
mod tests {
    use super::rm_paths;
    use indexa_core::store::Store;
    use indexa_core::walker::{Entry, EntryKind};
    use std::path::{Path, PathBuf};

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

    fn remaining(store: &Store) -> Vec<String> {
        let mut stmt = store
            .db_connection()
            .prepare("SELECT path FROM entries ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// `rm_paths` with its output captured.
    fn rm(
        store: &mut Store,
        paths: &[String],
        recursive: bool,
        yes: bool,
        is_tty: bool,
    ) -> (anyhow::Result<Option<usize>>, String) {
        let mut out = Vec::new();
        let result = rm_paths(store, paths, recursive, yes, is_tty, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    fn canonical(p: &Path) -> PathBuf {
        super::super::helpers::resolve_roots(vec![p.to_string_lossy().into_owned()], false)
            .unwrap()
            .remove(0)
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// F23: `rm -r` deleted whole subtrees with no confirmation of any kind. A
    /// non-interactive session without `--yes` must refuse before touching the index.
    #[test]
    fn recursive_rm_refuses_without_yes_when_not_a_tty() {
        let mut store = store_with(&["/proj/a.rs", "/proj/sub/b.rs"]);
        let err = rm(&mut store, &["/proj".into()], true, false, false)
            .0
            .unwrap_err();
        assert!(err.to_string().contains("--yes"), "got: {err}");
        assert_eq!(entry_count(&store), 2, "nothing may be deleted on refusal");
    }

    #[test]
    fn recursive_rm_with_yes_removes_the_subtree() {
        let mut store = store_with(&["/proj/a.rs", "/proj/sub/b.rs", "/other/c.rs"]);
        let n = rm(&mut store, &["/proj".into()], true, true, false)
            .0
            .unwrap();
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
        let n = rm(&mut store, &["/proj/a.rs".into()], false, false, false)
            .0
            .unwrap();
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
        let canon = canonical(&real);
        let a = s(&canon.join("a.rs"));
        let b = s(&canon.join("sub").join("b.rs"));
        let mut store = store_with(&[&a, &b]);

        // A non-canonical spelling of the same directory: real/sub/.. == real.
        let roundabout = real.join("sub").join("..");
        let file = s(&roundabout.join("a.rs"));
        let n = rm(&mut store, &[file], false, false, false).0.unwrap();
        assert_eq!(n, Some(1), "plain rm must find the canonical entry");

        let subtree = s(&roundabout.join("sub"));
        let n = rm(&mut store, &[subtree], true, true, false).0.unwrap();
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
        let canon = canonical(&real);
        let literal = s(&real.join("sub").join("..").join("a.rs"));
        let canonical = s(&canon.join("a.rs"));
        assert_ne!(literal, canonical);
        let mut store = store_with(&[&literal, &canonical]);

        let n = rm(
            &mut store,
            std::slice::from_ref(&literal),
            false,
            false,
            false,
        )
        .0
        .unwrap();
        assert_eq!(n, Some(1));
        assert_eq!(
            remaining(&store),
            [canonical],
            "the canonical entry must survive"
        );
    }

    /// PR #580 review M1: the `-r` refusal named the path as typed while the fallback then
    /// removed its canonical form. It must name the path that will actually be removed, say
    /// that it differs from what was typed, and give the entry count.
    #[test]
    fn recursive_refusal_names_the_canonical_path_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let canon = canonical(&real);
        let b = s(&canon.join("sub").join("b.rs"));
        let c = s(&canon.join("sub").join("c.rs"));
        let a = s(&canon.join("a.rs"));
        let mut store = store_with(&[&a, &b, &c]);

        let typed = s(&real.join("sub").join("..").join("sub"));
        let err = rm(&mut store, std::slice::from_ref(&typed), true, false, false)
            .0
            .unwrap_err()
            .to_string();
        let sub = s(&canon.join("sub"));
        for expected in [
            format!("{sub} (2 entries)"),
            format!("{typed} resolves to {sub}"),
        ] {
            assert!(err.contains(&expected), "want {expected:?} in: {err}");
        }
        assert!(err.contains("--yes"), "got: {err}");
        assert_eq!(entry_count(&store), 3, "nothing may be deleted on refusal");
    }

    /// PR #580 review L5: a relative `.` resolves against the working directory, and the
    /// refusal says so instead of showing a bare `.`.
    #[test]
    fn recursive_rm_of_dot_names_the_working_directory() {
        let cwd = canonical(&std::env::current_dir().unwrap());
        let x = s(&cwd.join("x.rs"));
        let y = s(&cwd.join("d").join("y.rs"));
        let mut store = store_with(&[&x, &y, "/elsewhere/z.rs"]);

        let err = rm(&mut store, &[".".into()], true, false, false)
            .0
            .unwrap_err()
            .to_string();
        let cwd_s = s(&cwd);
        for expected in [
            format!("{cwd_s} (2 entries)"),
            format!(". resolves to {cwd_s}"),
        ] {
            assert!(err.contains(&expected), "want {expected:?} in: {err}");
        }
        assert_eq!(entry_count(&store), 3);

        let (n, out) = rm(&mut store, &[".".into()], true, true, false);
        assert_eq!(n.unwrap(), Some(2));
        assert_eq!(remaining(&store), ["/elsewhere/z.rs"]);
        assert!(
            out.contains(&format!("Removed subtree: {} (resolved from .)", s(&cwd))),
            "got: {out}"
        );
    }

    /// PR #580 review L5: the canonical fallback keeps the subtree boundary — `.../b` never
    /// counts or removes `.../bc` — and the prompt's count matches what is removed.
    #[test]
    fn canonical_subtree_does_not_spill_into_a_sibling_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        for d in ["sub", "b", "bc"] {
            std::fs::create_dir_all(real.join(d)).unwrap();
        }
        let canon = canonical(&real);
        let bx = s(&canon.join("b").join("x.rs"));
        let by = s(&canon.join("b").join("y.rs"));
        let bcz = s(&canon.join("bc").join("z.rs"));
        let mut store = store_with(&[&bx, &by, &bcz]);

        let typed = s(&real.join("sub").join("..").join("b"));
        let err = rm(&mut store, std::slice::from_ref(&typed), true, false, false)
            .0
            .unwrap_err()
            .to_string();
        let expected = format!("{} (2 entries)", s(&canon.join("b")));
        assert!(err.contains(&expected), "want {expected:?} in: {err}");

        let n = rm(&mut store, &[typed], true, true, false).0.unwrap();
        assert_eq!(n, Some(2));
        assert_eq!(remaining(&store), [bcz], "the sibling prefix must survive");
    }

    /// PR #580 review M2: a plain `rm` of a symlink that is not itself indexed silently
    /// removed its target's entry, even one outside the link's own indexed root. Like `rm`
    /// on disk it must not follow a symlink in the last component without confirmation; a
    /// symlinked directory earlier in the path still resolves silently.
    #[cfg(unix)]
    #[test]
    fn plain_rm_does_not_follow_a_final_symlink_without_yes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("t.rs"), "t").unwrap();
        std::fs::write(outside.join("u.rs"), "u").unwrap();
        std::os::unix::fs::symlink(outside.join("t.rs"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dirlink")).unwrap();
        let a = s(&canonical(&root).join("a.rs"));
        let t = s(&canonical(&outside).join("t.rs"));
        let u = s(&canonical(&outside).join("u.rs"));
        let mut store = store_with(&[&a, &t, &u]);

        let link = s(&root.join("link"));
        let err = rm(&mut store, std::slice::from_ref(&link), false, false, false)
            .0
            .unwrap_err()
            .to_string();
        for expected in [format!("{link} resolves to {t}"), format!("{t} (1 entry)")] {
            assert!(err.contains(&expected), "want {expected:?} in: {err}");
        }
        assert!(err.contains("--yes"), "got: {err}");
        assert_eq!(entry_count(&store), 3, "nothing may be deleted on refusal");

        // Only the final component is guarded: a symlinked directory earlier in the path
        // resolves the way it does for every other command (macOS /tmp -> /private/tmp).
        let via_dir = s(&root.join("dirlink").join("u.rs"));
        let n = rm(&mut store, &[via_dir], false, false, false).0.unwrap();
        assert_eq!(n, Some(1));

        let n = rm(&mut store, &[link], false, true, false).0.unwrap();
        assert_eq!(n, Some(1), "--yes removes the symlink target");
        assert_eq!(remaining(&store), [a]);
    }

    /// PR #580 review M1: `rm -r ./link` where the link points at another indexed tree.
    /// The refusal names the target subtree, not just the link.
    #[cfg(unix)]
    #[test]
    fn recursive_refusal_names_a_symlinks_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let canon = canonical(&target);
        let a = s(&canon.join("a.rs"));
        let mut store = store_with(&[&a]);

        let err = rm(&mut store, &[s(&link)], true, false, false)
            .0
            .unwrap_err()
            .to_string();
        let canon = s(&canon);
        for expected in [
            format!("{} resolves to {canon}", s(&link)),
            format!("{canon} (1 entry)"),
        ] {
            assert!(err.contains(&expected), "want {expected:?} in: {err}");
        }
        assert_eq!(entry_count(&store), 1);
    }

    /// PR #580 review L4: `rm -r /`, `rm -r ""` or a whitespace path is a prefix of every
    /// entry. Refuse it without `--yes` even on a terminal, where a stray Enter-then-y
    /// would otherwise empty the index.
    #[test]
    fn recursive_rm_of_everything_needs_yes_even_on_a_tty() {
        let mut store = store_with(&["/proj/a.rs", "/other/b.rs"]);
        for path in ["/", "", "   "] {
            for is_tty in [false, true] {
                let err = rm(&mut store, &[path.into()], true, false, is_tty)
                    .0
                    .unwrap_err()
                    .to_string();
                assert!(
                    err.contains("matches every entry") && err.contains("--yes"),
                    "{path:?} (tty={is_tty}): {err}"
                );
                assert_eq!(entry_count(&store), 2);
            }
        }
        let n = rm(&mut store, &["/".into()], true, true, false).0.unwrap();
        assert_eq!(n, Some(2), "--yes still empties the index");
    }

    /// PR #580 review L4: the same guard applies when the canonical fallback lands on `/`.
    #[cfg(unix)]
    #[test]
    fn recursive_rm_through_a_link_to_root_needs_yes() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("to-root");
        std::os::unix::fs::symlink("/", &link).unwrap();
        let mut store = store_with(&["/proj/a.rs"]);

        let err = rm(&mut store, &[s(&link)], true, false, true)
            .0
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("resolves to /") && err.contains("matches every entry"),
            "got: {err}"
        );
        assert_eq!(entry_count(&store), 1);
    }

    /// PR #580 review L3: a recursive miss printed "Removed subtree: X (0 entries)".
    #[test]
    fn recursive_rm_miss_reports_not_found() {
        let mut store = store_with(&["/proj/a.rs"]);
        let (n, out) = rm(&mut store, &["/nope".into()], true, true, false);
        assert_eq!(n.unwrap(), Some(0));
        assert!(out.contains("Not found in index: /nope"), "got: {out}");
        assert!(!out.contains("Removed subtree"), "got: {out}");
    }
}
