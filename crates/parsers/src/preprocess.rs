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
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

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
        let mut child = cmd.spawn().ok()?;

        if let Some(mut stdin) = child.stdin.take() {
            // Best-effort: a tool that only reads argv[1] simply never reads stdin, and a
            // closed pipe on write is not a failure worth propagating.
            let _ = stdin.write_all(&bytes);
        }
        let mut stdout_pipe = child.stdout.take();
        let cap = self.max_output_bytes;
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.take(cap).read_to_end(&mut buf);
            }
            buf
        });

        let status = match child.wait_timeout(self.timeout).ok()? {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
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
