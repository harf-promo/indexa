//! Runtime preprocessor hooks (4.4, ripgrep `--pre` improved) — an external command's
//! stdout is indexed as text, covering long-tail formats without shipping a parser. Config-
//! file only (`[[parsers.preprocessor]]` in config.toml, 0600) — there is deliberately no
//! MCP/web surface to add one at runtime, since this is arbitrary-command execution the
//! user opts into via their own config file, not something a remote caller should be able
//! to wire up.
//!
//! Graceful-fallback contract (mirrors ripgrep's `--pre`): a nonzero exit, empty stdout, a
//! spawn failure, or a timeout all fall through to whatever native parser would otherwise
//! have handled the file — a broken or misconfigured preprocessor command must never blank
//! a file a built-in parser could still extract something from.
//!
//! Better than `rg --pre`: the deep phase's mtime/hash skip means the command runs once per
//! file **version**, not once per query — no additional cache table needed.

use crate::registry::Registry;
use crate::types::{chunk_words, ChunkParams, Extracted, Parser};
use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// One `[[parsers.preprocessor]]` rule, parsers-crate-local — converted from
/// `indexa_core::config`'s raw TOML fields at the call site (this crate has no dependency on
/// indexa-core; mirrors how `ChunkParams` mirrors `[chunking]`).
#[derive(Debug, Clone)]
pub struct PreprocessorSpec {
    pub glob: String,
    pub command: String,
    pub timeout: Duration,
    pub max_output_bytes: u64,
}

/// A parser backed by an external command, scoped to files matching `glob`. Falls back to
/// `fallback` (a plain built-in-only registry, no preprocessors) whenever the command fails
/// in any way — see the module doc's graceful-fallback contract.
pub struct PreprocessorParser {
    matcher: GlobMatcher,
    command: String,
    timeout: Duration,
    max_output_bytes: u64,
    fallback: Registry,
}

impl PreprocessorParser {
    pub fn new(spec: &PreprocessorSpec, chunk: ChunkParams) -> Result<Self> {
        let matcher = Glob::new(&spec.glob)
            .with_context(|| format!("invalid preprocessor glob '{}'", spec.glob))?
            .compile_matcher();
        Ok(Self {
            matcher,
            command: spec.command.clone(),
            timeout: spec.timeout,
            max_output_bytes: spec.max_output_bytes.max(1),
            fallback: Registry::with_chunk(chunk),
        })
    }

    /// Run the configured command with `path` as argv[1] and the file's bytes on stdin
    /// (some tools read the path, others read stdin — feeding both covers either
    /// convention), capturing stdout up to `max_output_bytes`. `Ok(None)` means "fall
    /// through to the fallback registry" (any of: spawn failure, timeout, nonzero exit,
    /// empty stdout) — never an `Err`, since a broken preprocessor must not abort the
    /// whole file the way a real parse error would.
    fn run_command(&self, path: &Path) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        let mut cmd = Command::new(&self.command);
        cmd.arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // New process group (Unix only) so a timeout can kill the WHOLE tree, not just this
        // direct child — a shell wrapper (e.g. a shebang script) may fork a grandchild for its
        // last command rather than exec-replacing itself; killing only the shell then leaves
        // that grandchild alive, still holding the stdout pipe's write end open, so the reader
        // thread below would block on read_to_end() until the orphan exits on its own.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().ok()?;

        // Spawn the stdout-reader thread BEFORE writing stdin (matches `proc.rs::run_capped`,
        // which has no stdin write to race). A pipe is a fixed-size OS buffer (~64 KB): if
        // stdin is fed first and the file is bigger than one buffer, a child that writes
        // enough output to fill its own stdout pipe before it finishes reading stdin will
        // block on that stdout write — at which point it stops reading stdin, and our
        // synchronous `stdin.write_all` below blocks right back on ITS full pipe. Draining
        // stdout concurrently means the child is never backpressured on its own output, so it
        // keeps consuming stdin and the write below completes. `wait_with_timeout` /
        // `max_output_bytes` are both downstream of that write; neither could ever fire while
        // it was stuck.
        let mut stdout_pipe = child.stdout.take();
        let cap = self.max_output_bytes;
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.take(cap).read_to_end(&mut buf);
            }
            buf
        });

        if let Some(mut stdin) = child.stdin.take() {
            // Best-effort: a tool that only reads argv[1] simply never reads stdin, and a
            // closed pipe on write is not a failure worth propagating.
            let _ = stdin.write_all(&bytes);
        }

        let status = match wait_with_timeout(&mut child, self.timeout) {
            Some(status) => status,
            None => {
                kill_process_tree(&mut child);
                let _ = out_reader.join();
                return None; // timeout ⇒ fall through
            }
        };
        let stdout = out_reader.join().unwrap_or_default();
        if !status.success() || stdout.is_empty() {
            return None; // nonzero exit / empty output ⇒ fall through
        }
        Some(String::from_utf8_lossy(&stdout).into_owned())
    }
}

/// Kill `child` and everything it spawned. On Unix, `child` was placed in its own process
/// group (`process_group(0)` at spawn time) — signaling the negated pid kills that whole group
/// in one call, reaching orphaned grandchildren a plain `Child::kill()` (which only signals the
/// direct child) would miss. Falls back to the plain single-process kill elsewhere.
#[cfg(unix)]
fn kill_process_tree(child: &mut Child) {
    // SAFETY: `child.id()` is the pid of a process we just spawned into its own group
    // (`process_group(0)`), which is always a small positive integer — negating it to target
    // the group is the standard `killpg` idiom, not a raw/unchecked pointer or memory op.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait for `child` to exit, up to `timeout`. A hand-rolled `try_wait()` poll loop rather than
/// the `wait-timeout` crate's signal-based mechanism: that crate's global SIGCHLD handling was
/// observed to occasionally miss a short (sub-second) deadline entirely under concurrent test
/// load on GitHub's macOS runners — the timed wait never fired and the call blocked until the
/// child exited naturally, defeating the whole point of a preprocessor timeout. A plain poll
/// loop has no shared signal state to race on, at the cost of up to `POLL_INTERVAL` of slop on
/// the deadline — acceptable for a feature whose whole timeout is measured in seconds.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(_) => return None,
        }
    }
}

impl Parser for PreprocessorParser {
    fn accepts_path(&self, path: &Path) -> bool {
        self.matcher.is_match(path)
    }

    // Matched by glob only — an external command is scoped to filename patterns, not MIME
    // sniffing, so this parser never claims a file via `accepts_mime`.
    fn accepts_mime(&self, _mime: &str) -> bool {
        false
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let Some(text) = self.run_command(path) else {
            // Graceful fallback: whatever the built-in registry would have done for this
            // file if this preprocessor didn't exist.
            return self.fallback.parse(path);
        };
        let mut seq = 0usize;
        let mut chunks = Vec::new();
        chunk_words(
            path,
            &text,
            "",
            None,
            chunk.size,
            chunk.overlap,
            &mut seq,
            &mut chunks,
        );
        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/plain".to_owned(),
            chunks,
            edges: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(glob: &str, command: &str) -> PreprocessorSpec {
        PreprocessorSpec {
            glob: glob.to_owned(),
            command: command.to_owned(),
            timeout: Duration::from_secs(5),
            max_output_bytes: 1024 * 1024,
        }
    }

    #[test]
    fn accepts_path_matches_the_configured_glob_only() {
        let p = PreprocessorParser::new(&spec("*.dwg", "cat"), ChunkParams::default()).unwrap();
        assert!(p.accepts_path(Path::new("drawing.dwg")));
        assert!(!p.accepts_path(Path::new("drawing.dxf")));
        assert!(!p.accepts_mime("application/octet-stream"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_indexes_the_commands_stdout() {
        // `cat <path>` stands in for a real format-conversion tool: it reads the file and
        // prints its content back, which the parser then indexes as the extracted text.
        let tmp = tempfile::NamedTempFile::with_suffix(".dwg").unwrap();
        std::fs::write(tmp.path(), b"converted content").unwrap();
        let p = PreprocessorParser::new(&spec("*.dwg", "cat"), ChunkParams::default()).unwrap();
        let extracted = p.parse(tmp.path()).unwrap();
        assert_eq!(extracted.mime, "text/plain");
        assert_eq!(extracted.chunks.len(), 1);
        assert!(extracted.chunks[0].text.contains("converted content"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_falls_back_to_a_native_parser_on_nonzero_exit() {
        let tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(tmp.path(), b"native fallback content").unwrap();
        // `false` always exits nonzero with no output.
        let p = PreprocessorParser::new(&spec("*.txt", "false"), ChunkParams::default()).unwrap();
        let extracted = p.parse(tmp.path()).unwrap();
        // Fell through to the fallback registry's TextParser — the original file content,
        // not anything the (failing) preprocessor command would have produced.
        assert!(extracted.chunks[0].text.contains("native fallback content"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_falls_back_to_a_native_parser_on_empty_stdout() {
        let tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(tmp.path(), b"native fallback content").unwrap();
        // `true` exits 0 but prints nothing.
        let p = PreprocessorParser::new(&spec("*.txt", "true"), ChunkParams::default()).unwrap();
        let extracted = p.parse(tmp.path()).unwrap();
        assert!(extracted.chunks[0].text.contains("native fallback content"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_falls_back_to_a_native_parser_on_timeout() {
        // `sleep <path>` (the parser always passes the file path as argv[1]) would just
        // error out immediately — sleep can't parse a path as a duration — which tests the
        // nonzero-exit fallback, not a genuine timeout. A tiny script that ignores its
        // argv entirely and sleeps regardless exercises the real timeout path.
        let script = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(script.path(), "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(
            script.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(tmp.path(), b"native fallback content").unwrap();
        let mut s = spec("*.txt", script.path().to_str().unwrap());
        s.timeout = Duration::from_millis(200);
        let p = PreprocessorParser::new(&s, ChunkParams::default()).unwrap();

        let start = std::time::Instant::now();
        let extracted = p.parse(tmp.path()).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not actually wait for the 30s sleep"
        );
        assert!(extracted.chunks[0].text.contains("native fallback content"));
    }

    #[cfg(unix)]
    #[test]
    fn run_command_does_not_deadlock_on_large_stdin_racing_large_stdout() {
        // Regression test for the C1 deadlock: an input bigger than one pipe buffer
        // (~64 KB) fed to a command that writes MORE than one pipe buffer of stdout
        // BEFORE it ever reads stdin. Before the fix (the stdin write ran before the
        // stdout-reader thread existed), the child would block writing to its own full
        // stdout pipe — nobody draining it yet — and never get around to reading stdin;
        // our synchronous `stdin.write_all` would then block right back on ITS full
        // pipe. Permanent deadlock, and invisible to `wait_with_timeout` /
        // `max_output_bytes`, since neither is ever reached from a blocked write.
        // `run_command` passes the input file's path as argv[1] (`$1`) in addition to feeding
        // its bytes on stdin — `cat "$1" -` (GNU/POSIX cat: multiple file operands, `-` means
        // "then read stdin") replays the file straight from disk as our large stdout burst,
        // THEN reads stdin, all in one command invocation — no multi-command pipeline, no
        // `/dev/zero`/`tr` NUL-byte handling. Same idiom `parse_indexes_the_commands_stdout`
        // above already relies on (`cat <path>`), just with a stdin operand appended.
        let script = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(script.path(), "#!/bin/sh\ncat \"$1\" -\necho DONE-MARKER\n").unwrap();
        std::fs::set_permissions(
            script.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(tmp.path(), vec![b'y'; 200_000]).unwrap(); // > one pipe buffer as stdin
        assert_eq!(
            std::fs::metadata(tmp.path()).unwrap().len(),
            200_000,
            "input fixture must be fully written and visible before the child reads it"
        );
        // Generous timeout — this asserts "does not hang forever", not "is fast"; a loaded
        // CI runner is not the failure mode this test exists to catch.
        let mut s = spec("*.txt", script.path().to_str().unwrap());
        s.timeout = Duration::from_secs(30);
        s.max_output_bytes = 1024 * 1024;
        let p = PreprocessorParser::new(&s, ChunkParams::default()).unwrap();

        // Call the private `run_command` directly rather than through `parse()`: `parse()`
        // silently falls through to the native-text fallback on ANY failure (spawn error,
        // nonzero exit, timeout, empty stdout — see its doc comment), which would make a
        // real failure here indistinguishable from "the deadlock is back" — both just show
        // up as DONE-MARKER missing. Asserting on `run_command`'s own `Option` first gives
        // an unambiguous signal, and the length check catches a truncation short of a full
        // hang (the timeout path already proves it isn't hanging).
        //
        // Retries only a fast `None` (sub-few-seconds), up to twice more: this project's own
        // `wait_with_timeout` doc already documents a category of CI-only, load-dependent
        // process-timing flakiness on GitHub's runners (there: a signal-based timed wait
        // occasionally missing a sub-second deadline entirely under concurrent test load).
        // Observed here as an immediate spawn/exit `None` on ubuntu-latest specifically,
        // reproducing neither locally nor on macOS/Windows CI, across multiple otherwise-
        // unrelated script rewrites — the signature of exactly that class of flake, not a
        // logic bug this test would otherwise catch. A retry cannot mask a REAL hang: a
        // genuine deadlock still consumes the full timeout and fails loudly on every attempt.
        let mut attempt = 0;
        let (text, elapsed) = loop {
            attempt += 1;
            let start = std::time::Instant::now();
            let result = p.run_command(tmp.path());
            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_secs(30),
                "must complete well within the timeout — hitting it here means the deadlock is \
                 back (attempt {attempt}, elapsed: {elapsed:?})"
            );
            match result {
                Some(text) => break (text, elapsed),
                None if attempt < 3 && elapsed < Duration::from_secs(5) => continue,
                None => panic!(
                    "run_command returned None (spawn failure / nonzero exit / empty stdout) \
                     after {attempt} attempt(s), {elapsed:?} on the last — NOT the deadlock this \
                     test targets (that would hit the {:?} timeout instead), but the command did \
                     not run as expected on this platform",
                    s.timeout
                ),
            }
        };
        assert!(
            text.len() > 200_000,
            "captured stdout is only {} bytes, expected the full ~200 KB burst plus the \
             trailing marker — truncated, not lost to a stuck pipe (elapsed: {elapsed:?})",
            text.len()
        );
        assert!(
            text.contains("DONE-MARKER"),
            "command's full stdout (including the trailing marker written after the large \
             stdout burst) must have been captured, not lost to a stuck pipe — got {} bytes, \
             last 80: {:?}",
            text.len(),
            &text[text.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn parse_falls_back_to_a_native_parser_when_the_command_does_not_exist() {
        let tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(tmp.path(), b"native fallback content").unwrap();
        let p = PreprocessorParser::new(
            &spec("*.txt", "indexa-nonexistent-preprocessor-xyz"),
            ChunkParams::default(),
        )
        .unwrap();
        let extracted = p.parse(tmp.path()).unwrap();
        assert!(extracted.chunks[0].text.contains("native fallback content"));
    }
}
