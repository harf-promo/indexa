//! Co-change pair computation from git history (2.7) — files that historically change
//! together, a behavioral-coupling signal invisible to static analysis. No git2
//! dependency: shells out to `git log`, mirroring [`crate::gitdiff`].

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Default commit-history depth: enough to be a meaningful signal without an unbounded
/// `git log` parse on a large repo.
pub const DEFAULT_COMMIT_LIMIT: usize = 2000;
/// Commits touching more than this many files are skipped entirely — a large refactor
/// or merge would otherwise flood every pair it touches with noise.
pub const MAX_FILES_PER_COMMIT: usize = 50;

/// A commit-boundary marker that can never collide with a real file path (paths never
/// contain a NUL byte).
const BOUNDARY: &str = "\0";

/// Resolve the repository top-level directory for `root` via `git rev-parse
/// --show-toplevel` — `git log --name-only` paths are always repo-root-relative, even
/// when scoped to a subdirectory pathspec, so joining onto `root` itself (when `root` is
/// a subdirectory) would double up the subdirectory segment. Falls back to `root` on any
/// failure (fail-open — the caller's own git invocation surfaces the real error).
fn repo_toplevel(root: &Path) -> std::path::PathBuf {
    let Some(root_str) = root.to_str() else {
        return root.to_owned();
    };
    Command::new("git")
        .arg("-C")
        .arg(root_str)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| std::path::PathBuf::from(s.trim()))
        .unwrap_or_else(|| root.to_owned())
}

/// Run `git log --name-only --no-merges -n <commit_limit>` scoped to files under `root`
/// and count how often each pair of files appears together in the same commit (commits
/// touching more than [`MAX_FILES_PER_COMMIT`] files are skipped). Returns
/// `(path_a, path_b, count)` triples with absolute paths, in no particular pair order —
/// canonicalizing `path_a <= path_b` is
/// [`indexa_core::store::Store::replace_co_change`]'s job, not this parser's. Fails
/// open: a non-repo root or missing `git` binary returns an empty vec, not an error.
pub fn co_change_pairs(root: &Path, commit_limit: usize) -> Result<Vec<(String, String, i64)>> {
    let Some(root_str) = root.to_str() else {
        return Ok(Vec::new());
    };
    let toplevel = repo_toplevel(root);
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root_str)
        .args([
            "log",
            "--name-only",
            "--no-merges",
            "--pretty=format:%x00",
            &format!("-n{commit_limit}"),
            // Scope to files under `root` — without a pathspec, `-C root` only picks
            // which repo to use, and `git log` still walks the WHOLE repo's history.
            "--",
            ".",
        ])
        .output()
    else {
        return Ok(Vec::new());
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut counts: HashMap<(String, String), i64> = HashMap::new();
    let mut current: Vec<String> = Vec::new();
    let flush = |current: &mut Vec<String>, counts: &mut HashMap<(String, String), i64>| {
        if current.len() > 1 && current.len() <= MAX_FILES_PER_COMMIT {
            current.sort();
            current.dedup();
            for i in 0..current.len() {
                for j in (i + 1)..current.len() {
                    *counts
                        .entry((current[i].clone(), current[j].clone()))
                        .or_insert(0) += 1;
                }
            }
        }
        current.clear();
    };

    for line in text.lines() {
        if line == BOUNDARY {
            flush(&mut current, &mut counts);
        } else if !line.is_empty() {
            // Repo-root-relative, per the `repo_toplevel` doc comment above — NOT relative
            // to `root` when `root` is a subdirectory.
            let abs = toplevel
                .join(line)
                .to_string_lossy()
                .into_owned()
                .replace('\\', "/");
            current.push(abs);
        }
    }
    flush(&mut current, &mut counts);

    Ok(counts.into_iter().map(|((a, b), c)| (a, b, c)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn run(root: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn co_change_pairs_counts_files_touched_together_and_skips_large_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.email", "t@example.com"]);
        run(root, &["config", "user.name", "t"]);
        run(root, &["config", "commit.gpgsign", "false"]);

        // Commit 1: a.rs + b.rs together (co-change pair).
        std::fs::write(root.join("a.rs"), "1").unwrap();
        std::fs::write(root.join("b.rs"), "1").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-q", "-m", "c1"]);

        // Commit 2: a.rs + b.rs again — count should be 2.
        std::fs::write(root.join("a.rs"), "2").unwrap();
        std::fs::write(root.join("b.rs"), "2").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-q", "-m", "c2"]);

        // Commit 3: c.rs alone — no pair.
        std::fs::write(root.join("c.rs"), "1").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-q", "-m", "c3"]);

        let pairs = co_change_pairs(root, DEFAULT_COMMIT_LIMIT).unwrap();
        let ab = pairs
            .iter()
            .find(|(a, b, _)| {
                (a.ends_with("a.rs") && b.ends_with("b.rs"))
                    || (a.ends_with("b.rs") && b.ends_with("a.rs"))
            })
            .map(|(_, _, c)| *c);
        assert_eq!(ab, Some(2), "a.rs/b.rs co-changed in 2 commits");
        assert!(
            !pairs
                .iter()
                .any(|(a, b, _)| a.ends_with("c.rs") || b.ends_with("c.rs")),
            "c.rs never co-occurred with anything — no pair should mention it"
        );
    }

    #[test]
    fn co_change_pairs_on_a_non_repo_root_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(co_change_pairs(tmp.path(), DEFAULT_COMMIT_LIMIT)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn co_change_pairs_scoped_to_a_subdirectory_resolves_correct_absolute_paths() {
        // Regression test: `git log --name-only` paths are always repo-root-relative,
        // even when `root` (passed via `-C`) is a subdirectory — joining onto the
        // subdirectory itself would double up the segment
        // (sub/sub/a.rs instead of sub/a.rs).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.email", "t@example.com"]);
        run(root, &["config", "user.name", "t"]);
        run(root, &["config", "commit.gpgsign", "false"]);

        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.rs"), "1").unwrap();
        std::fs::write(sub.join("b.rs"), "1").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-q", "-m", "c1"]);

        // Scope to the SUBDIRECTORY, not the repo root.
        let pairs = co_change_pairs(&sub, DEFAULT_COMMIT_LIMIT).unwrap();
        assert_eq!(pairs.len(), 1);
        let (a, b, count) = &pairs[0];
        assert_eq!(*count, 1);
        // Check the path SUFFIX only, not a canonicalized absolute-path equality: on
        // Windows, `Path::canonicalize()` prepends the `\\?\` extended-length-path
        // marker, which git's own output never has — comparing full equality there
        // would fail on a Rust std quirk, not on anything this test is checking (the
        // segment must appear exactly once, not doubled as "sub/sub/a.rs").
        let single_sub = |p: &str| {
            let norm = p.replace('\\', "/");
            let count = norm.matches("/sub/").count();
            assert_eq!(count, 1, "\"sub\" must appear exactly once, got: {p}");
            assert!(
                norm.ends_with("/sub/a.rs") || norm.ends_with("/sub/b.rs"),
                "must resolve under the repo root's sub/ dir, got: {p}"
            );
        };
        single_sub(a);
        single_sub(b);
    }
}
