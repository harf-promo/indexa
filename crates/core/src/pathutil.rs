//! Path-structure helpers shared across the indexing pipeline.
//!
//! These back the summary queue's deepest-first ordering and incremental
//! re-summarization. They are load-bearing: the CLI watcher, the web watcher,
//! and `summarize`'s enqueue pass all depend on computing the *same* depth and
//! ancestor chain, so they live here rather than being copy-pasted per crate.

use std::path::{Path, PathBuf};

/// Path's `/`-or-`\`-separator count — the depth metric the summary queue sorts
/// on (deepest first), so re-pended ancestors roll up after their children.
pub fn path_depth(path: &str) -> i64 {
    path.chars().filter(|&c| c == '/' || c == '\\').count() as i64
}

/// The ancestor directories of `path`, from its immediate parent up to and
/// including the watched root that contains it. A changed file makes every
/// roll-up on this chain stale, so each is re-queued for the worker.
///
/// Returns empty when `path` is outside every root, or when the matching root
/// is itself a file (a file-as-root degenerates cleanly to no ancestor dirs —
/// it must not walk up to the filesystem root).
pub fn ancestor_dirs_to_root(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(root) = roots.iter().find(|r| path.starts_with(r)) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut cur = path.parent();
    while let Some(d) = cur {
        // Stay within the watched subtree: stop if we've walked above the root.
        if !d.starts_with(root) {
            break;
        }
        dirs.push(d.to_path_buf());
        if d == root.as_path() {
            break;
        }
        cur = d.parent();
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_depth_counts_separators() {
        assert_eq!(path_depth("/a/b/c.txt"), 3);
        assert_eq!(path_depth("/a"), 1);
        assert_eq!(path_depth("rel"), 0);
    }

    #[test]
    fn ancestor_dirs_walks_up_to_and_includes_root() {
        let roots = vec![PathBuf::from("/proj")];
        let dirs = ancestor_dirs_to_root(Path::new("/proj/src/mod/file.rs"), &roots);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/proj/src/mod"),
                PathBuf::from("/proj/src"),
                PathBuf::from("/proj"),
            ]
        );
    }

    #[test]
    fn ancestor_dirs_file_directly_in_root() {
        let roots = vec![PathBuf::from("/proj")];
        assert_eq!(
            ancestor_dirs_to_root(Path::new("/proj/file.rs"), &roots),
            vec![PathBuf::from("/proj")]
        );
    }

    #[test]
    fn ancestor_dirs_empty_when_outside_any_root() {
        let roots = vec![PathBuf::from("/proj")];
        assert!(ancestor_dirs_to_root(Path::new("/other/file.rs"), &roots).is_empty());
    }

    #[test]
    fn ancestor_dirs_empty_when_root_is_a_file() {
        // Degenerate: a file passed as the watched root → no ancestor dirs to enqueue
        // (must not walk up to the filesystem root).
        let roots = vec![PathBuf::from("/proj/solo.txt")];
        assert!(ancestor_dirs_to_root(Path::new("/proj/solo.txt"), &roots).is_empty());
    }
}
