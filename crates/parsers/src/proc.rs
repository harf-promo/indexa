//! Subprocess execution with a hard timeout — kills a hung child instead of blocking indexing
//! forever.
//!
//! The parsers crate is synchronous (no tokio), so external tools (`pdftoppm`, `tesseract`,
//! whisper, `ffprobe`, `ffmpeg`) run via `std::process` and would otherwise `.output()` /
//! `.status()` with no upper bound. A malformed input that hangs one of these tools would hang the
//! whole `deep`/index run. [`run_capped`] waits with a timeout and, on expiry, kills the child so
//! the pipeline fails open (the file is skipped) rather than stalling.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Per-page `tesseract` OCR cap.
pub const TESSERACT_TIMEOUT: Duration = Duration::from_secs(60);
/// Whole-document `pdftoppm` rasterisation cap.
pub const PDFTOPPM_TIMEOUT: Duration = Duration::from_secs(120);
/// Whisper transcription cap. CPU transcription runs roughly realtime, so a long recording is
/// legitimately slow — this is a runaway backstop, not a throughput target.
pub const WHISPER_TIMEOUT: Duration = Duration::from_secs(1800);
/// `ffprobe` metadata cap (should be near-instant).
pub const FFPROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// `ffmpeg` frame-extraction cap.
pub const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);

/// Captured result of a capped subprocess run — mirrors the fields of [`std::process::Output`]
/// that callers use.
#[derive(Debug)]
pub struct CappedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `cmd` to completion or kill it after `timeout`, returning its captured output.
///
/// stdout and stderr are drained on **separate threads** so a child that fills an OS pipe buffer
/// can't deadlock the wait: a child blocked writing to a full pipe never exits, so without a
/// concurrent reader the timeout would never fire. On timeout the child is killed (which closes
/// its pipes, letting the reader threads finish) and an [`std::io::ErrorKind::TimedOut`] error is
/// returned; a spawn failure propagates as-is.
pub fn run_capped(mut cmd: Command, timeout: Duration) -> std::io::Result<CappedOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // Take the pipes and drain them concurrently with the timed wait.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            // Timed out — kill the child (closing its pipes so the readers unblock), reap it, and
            // report the timeout so the caller falls open and skips the file.
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "subprocess exceeded its timeout and was killed",
            ));
        }
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(CappedOutput {
        status,
        stdout,
        stderr,
    })
}

// Unix-only: these use `printf`/`sleep`, which aren't guaranteed on the Windows CI runner.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn run_capped_returns_output_within_timeout() {
        let mut cmd = Command::new("printf");
        cmd.arg("hello");
        let out = run_capped(cmd, Duration::from_secs(5)).expect("printf should run");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn run_capped_kills_on_timeout() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let r = run_capped(cmd, Duration::from_millis(300));
        assert!(r.is_err(), "a 30s sleep must time out");
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must return promptly after killing the child"
        );
    }
}
