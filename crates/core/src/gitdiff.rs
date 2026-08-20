//! Git diff parsing for `changed_impact` (2.3) — "what did I just touch and what does it
//! break". No git2 dependency: shells out to the user's own `git` binary and parses
//! unified-diff hunk headers, mirroring GitNexus's `detect_changes` and
//! codebase-memory-mcp's change-driven reindex, but read-only and on-demand.
//!
//! Fails open throughout: a missing `git` binary, a non-repo root, or a timed-out child
//! all resolve to an empty hunk list rather than an error — `changed_impact` degrades to
//! "nothing changed" instead of failing the whole tool call.

use anyhow::Result;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// `git diff` is a local, no-network operation — a hang here means a broken repo state
/// (e.g. a stale index.lock), not slow I/O. Bounded well above any real run.
const GIT_DIFF_TIMEOUT: Duration = Duration::from_secs(10);

/// One changed hunk: `path` (absolute, matching how the `edges`/`symbols` tables store
/// paths) plus the 1-based inclusive line range on the **new** side of the diff — the
/// side that matches what's currently on disk (and so what `symbols_overlapping` was
/// extracted from on the last `indexa deep`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedHunk {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// Where to diff against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffScope {
    /// Working tree vs the index (`git diff`) — uncommitted, unstaged edits.
    Unstaged,
    /// Index vs HEAD (`git diff --staged`) — what a commit right now would contain.
    Staged,
    /// Working tree vs an arbitrary ref (`git diff <ref>`) — branch, tag, or commit.
    Ref(String),
}

impl DiffScope {
    /// Parse an MCP/CLI `scope` string. Empty or `"unstaged"` ⇒ [`Self::Unstaged`];
    /// `"staged"` ⇒ [`Self::Staged`]; anything else is treated as a git ref (validated by
    /// `git` itself — an unknown ref surfaces as a git error, not a silent empty diff).
    pub fn parse(s: &str) -> Self {
        match s {
            "" | "unstaged" => DiffScope::Unstaged,
            "staged" => DiffScope::Staged,
            other => DiffScope::Ref(other.to_owned()),
        }
    }
}

/// Run `git -C root diff --unified=0` for `scope` and return the changed hunks, each
/// path resolved to an absolute path under `root`. Empty (not an error) when `root` isn't
/// inside a git repo, there's no `git` binary on `PATH`, or there are no changes —
/// `changed_impact` treats all three identically ("nothing changed here").
pub fn changed_hunks(root: &Path, scope: &DiffScope) -> Result<Vec<ChangedHunk>> {
    let Some(root_str) = root.to_str() else {
        return Ok(Vec::new());
    };
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root_str)
        .args(["diff", "--unified=0", "--no-color"]);
    match scope {
        DiffScope::Unstaged => {}
        DiffScope::Staged => {
            cmd.arg("--staged");
        }
        DiffScope::Ref(r) => {
            cmd.arg(r);
        }
    }
    let Ok(output) = run_capped(cmd, GIT_DIFF_TIMEOUT) else {
        return Ok(Vec::new());
    };
    if !output.status.success() {
        // Non-repo root, bad ref, no git installed with a usable exit code, etc. — fail open.
        return Ok(Vec::new());
    }
    let diff = String::from_utf8_lossy(&output.stdout);
    Ok(parse_unified_diff(&diff, root))
}

/// Parse `git diff --unified=0` output into per-file changed line ranges on the new side.
/// Hunks with a zero-length new range (pure deletions) are skipped — there's no line left
/// to map to a symbol in the current file content.
fn parse_unified_diff(diff: &str, root: &Path) -> Vec<ChangedHunk> {
    let mut out = Vec::new();
    let mut current_path: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            current_path = diff_new_path(rest).map(|rel| {
                root.join(rel)
                    .to_string_lossy()
                    .into_owned()
                    .replace('\\', "/")
            });
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            let Some(path) = current_path.clone() else {
                continue;
            };
            if let Some((start, len)) = parse_hunk_new_range(rest) {
                if len == 0 {
                    continue;
                }
                out.push(ChangedHunk {
                    path,
                    start_line: start,
                    end_line: start + len - 1,
                });
            }
        }
    }
    out
}

/// Extract the repo-relative path from a `+++ b/path/to/file` diff header line.
/// `/dev/null` (the new side of a deleted file) yields `None`.
fn diff_new_path(rest: &str) -> Option<&str> {
    let rest = rest.trim_end();
    if rest == "/dev/null" {
        return None;
    }
    rest.strip_prefix("b/").or(Some(rest))
}

/// Parse the `+start,len` (or bare `+start`, implying `len == 1`) half of a
/// `@@ -a,b +c,d @@ ...` hunk header.
fn parse_hunk_new_range(rest: &str) -> Option<(i64, i64)> {
    let plus = rest.find('+')?;
    let after = &rest[plus + 1..];
    let end = after.find(' ').unwrap_or(after.len());
    let spec = &after[..end];
    match spec.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => spec.parse().ok().map(|start: i64| (start, 1)),
    }
}

/// Captured result of a capped subprocess run.
struct CappedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

/// Run `cmd` to completion or kill it after `timeout` — the same drain-on-separate-threads
/// pattern as `indexa_parsers::proc::run_capped` (duplicated in miniature here rather than
/// shared, since `indexa-core` doesn't otherwise depend on `indexa-parsers`).
fn run_capped(mut cmd: Command, timeout: Duration) -> std::io::Result<CappedOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let mut out_pipe = child.stdout.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "git diff exceeded its timeout and was killed",
            ));
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    Ok(CappedOutput { status, stdout })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parse_maps_known_strings_and_falls_back_to_ref() {
        assert_eq!(DiffScope::parse(""), DiffScope::Unstaged);
        assert_eq!(DiffScope::parse("unstaged"), DiffScope::Unstaged);
        assert_eq!(DiffScope::parse("staged"), DiffScope::Staged);
        assert_eq!(DiffScope::parse("main"), DiffScope::Ref("main".to_owned()));
        assert_eq!(
            DiffScope::parse("HEAD~3"),
            DiffScope::Ref("HEAD~3".to_owned())
        );
    }

    #[test]
    fn diff_new_path_strips_b_prefix_and_flags_dev_null() {
        assert_eq!(diff_new_path("b/src/main.rs"), Some("src/main.rs"));
        assert_eq!(diff_new_path("/dev/null"), None);
        // Some git configs omit the a/ b/ prefixes entirely.
        assert_eq!(diff_new_path("src/main.rs"), Some("src/main.rs"));
    }

    #[test]
    fn parse_hunk_new_range_handles_explicit_and_implicit_length() {
        assert_eq!(parse_hunk_new_range("-5,2 +5,3 @@"), Some((5, 3)));
        // No comma ⇒ length 1.
        assert_eq!(parse_hunk_new_range("-5 +5 @@"), Some((5, 1)));
        // Zero-length new range (pure deletion) is still parsed — callers filter it.
        assert_eq!(parse_hunk_new_range("-5,2 +4,0 @@"), Some((4, 0)));
        assert_eq!(parse_hunk_new_range("garbage"), None);
    }

    #[test]
    fn parse_unified_diff_extracts_ranges_and_skips_pure_deletions() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,0 +11,2 @@ fn old_context() {
+added line one
+added line two
@@ -20,3 +23,0 @@ fn deleted_only() {
-removed a
-removed b
-removed c
diff --git a/src/b.rs b/src/b.rs
new file mode 100644
--- /dev/null
+++ b/src/b.rs
@@ -0,0 +1,5 @@
+new file content
";
        let root = Path::new("/repo");
        let hunks = parse_unified_diff(diff, root);
        assert_eq!(
            hunks,
            vec![
                ChangedHunk {
                    path: "/repo/src/a.rs".to_owned(),
                    start_line: 11,
                    end_line: 12,
                },
                ChangedHunk {
                    path: "/repo/src/b.rs".to_owned(),
                    start_line: 1,
                    end_line: 5,
                },
            ],
            "the pure-deletion hunk (+23,0) must be skipped — no new-side line to map"
        );
    }

    #[cfg(unix)]
    #[test]
    fn changed_hunks_reports_a_real_unstaged_edit_in_a_scratch_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        // The user's global gitconfig may require GPG-signed commits (1Password SSH agent) —
        // irrelevant to this throwaway scratch repo and unreachable in a sandboxed test run.
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("f.rs"), "fn a() {}\n").unwrap();
        run(&["add", "f.rs"]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(root.join("f.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let hunks = changed_hunks(root, &DiffScope::Unstaged).unwrap();
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].path.ends_with("f.rs"));
        assert_eq!((hunks[0].start_line, hunks[0].end_line), (2, 2));

        // No changes at all ⇒ empty, not an error.
        run(&["add", "f.rs"]);
        run(&["commit", "-q", "-m", "second"]);
        assert!(changed_hunks(root, &DiffScope::Unstaged)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn changed_hunks_on_a_non_repo_root_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let hunks = changed_hunks(tmp.path(), &DiffScope::Unstaged).unwrap();
        assert!(hunks.is_empty());
    }
}
