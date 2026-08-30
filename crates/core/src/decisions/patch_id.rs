//! Durable, content-based decision references (v0.78): a `git patch-id` computed from a
//! decision subject's currently-committed content, so a `record_decision` ledger entry keeps
//! resolving after the commit that captured it is rebased or squashed — a patch-id hashes diff
//! CONTENT, not a commit SHA, so it survives history rewrites a raw `git rev-parse HEAD`
//! reference would not.
//!
//! No git2/gix dependency: shells out to the user's own `git` binary, mirroring
//! [`crate::gitdiff`]'s `Command::new("git")` house style (duplicated in miniature here rather
//! than shared — this module doesn't otherwise depend on `gitdiff`, same reasoning `gitdiff.rs`
//! itself gives for not sharing with `indexa-parsers::proc::run_capped`).
//!
//! Fails open throughout: a missing `git` binary, a non-repo directory, an untracked or
//! vanished path, or a timed-out child all resolve to `None` — `record_decision` degrades to
//! "no durable reference available" rather than failing the whole tool call.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Same bound as `gitdiff::GIT_DIFF_TIMEOUT` — local, no-network git calls; a hang here means a
/// broken repo state (e.g. a stale index.lock), not slow I/O.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Git's well-known empty-tree object id. Diffing HEAD against it turns "this path's current
/// committed content" into a single patch with no dependency on which commit introduced it —
/// that's what makes the resulting patch-id stable across a rebase or squash.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Compute a durable patch-id for `subject_path`'s current HEAD content, running `git` with
/// `anchor_dir` as its working directory (git discovers the repo root upward from there, so
/// this need not be the repo root itself). `None` when `anchor_dir` isn't inside a git repo,
/// `subject_path` isn't tracked at HEAD (nothing committed yet, or it never existed), or
/// `git`/`git patch-id` aren't available.
pub fn compute_patch_id(anchor_dir: &Path, subject_path: &Path) -> Option<String> {
    let anchor = anchor_dir.to_str()?;
    let subject = subject_path.to_str()?;

    // Leg 1: the file's whole current content as one patch (diff against the empty tree).
    let mut diff_cmd = Command::new("git");
    diff_cmd
        .arg("-C")
        .arg(anchor)
        .args(["diff", EMPTY_TREE, "HEAD", "--", subject])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut diff_child = diff_cmd.spawn().ok()?;
    let diff_stdout = diff_child.stdout.take()?;

    // Leg 2: patch-id hashes that patch's content — piped directly from leg 1's stdout via the
    // OS pipe, never buffered through this process.
    let mut id_cmd = Command::new("git");
    id_cmd
        .arg("-C")
        .arg(anchor)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::from(diff_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut id_child = id_cmd.spawn().ok()?;
    let mut id_stdout = id_child.stdout.take();
    // Read on a separate thread so `wait_timeout` below can enforce the bound concurrently —
    // reading to EOF first would block until id_child exits, defeating the timeout.
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = id_stdout.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    // Bound both legs of the pipeline independently — a hang in either process must not block
    // the caller forever.
    let diff_ok = matches!(diff_child.wait_timeout(GIT_TIMEOUT), Ok(Some(s)) if s.success());
    if !diff_ok {
        let _ = diff_child.kill();
        let _ = diff_child.wait();
    }
    let id_ok = match id_child.wait_timeout(GIT_TIMEOUT) {
        Ok(Some(s)) => s.success(),
        _ => {
            let _ = id_child.kill();
            let _ = id_child.wait();
            false
        }
    };
    let out = out_reader.join().unwrap_or_default();
    if !diff_ok || !id_ok {
        return None;
    }
    let text = String::from_utf8_lossy(&out);
    let id = text.split_whitespace().next()?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(root: &Path) {
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
    }

    // Only used by the two `#[cfg(unix)]` tests below — gating it the same way keeps a
    // non-unix build (e.g. Windows CI) from seeing it as dead code under `-D warnings`.
    #[cfg(unix)]
    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[cfg(unix)]
    #[test]
    fn patch_id_is_stable_across_a_content_preserving_amend() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo(root);
        std::fs::write(root.join("f.rs"), "fn a() {}\n").unwrap();
        git(root, &["add", "f.rs"]);
        git(root, &["commit", "-q", "-m", "first message"]);

        let id1 = compute_patch_id(root, Path::new("f.rs")).unwrap();

        // Amend: same content, different commit SHA (and different message) — a rebase or
        // squash does the same thing to real history.
        git(root, &["commit", "--amend", "-q", "-m", "second message"]);
        let id2 = compute_patch_id(root, Path::new("f.rs")).unwrap();
        assert_eq!(
            id1, id2,
            "patch-id must survive a content-preserving history rewrite"
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_id_changes_when_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo(root);
        std::fs::write(root.join("f.rs"), "fn a() {}\n").unwrap();
        git(root, &["add", "f.rs"]);
        git(root, &["commit", "-q", "-m", "init"]);
        let id1 = compute_patch_id(root, Path::new("f.rs")).unwrap();

        std::fs::write(root.join("f.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        git(root, &["add", "f.rs"]);
        git(root, &["commit", "-q", "-m", "grow"]);
        let id2 = compute_patch_id(root, Path::new("f.rs")).unwrap();
        assert_ne!(
            id1, id2,
            "different committed content must yield a different patch-id"
        );
    }

    #[test]
    fn non_repo_dir_is_none_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(compute_patch_id(tmp.path(), Path::new("f.rs")).is_none());
    }

    #[test]
    fn untracked_path_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert!(compute_patch_id(tmp.path(), Path::new("never-existed.rs")).is_none());
    }
}
