//! A child spawned through `run_capped` must never inherit the indexer's stdin (ffmpeg and
//! whisper read the terminal, so keystrokes alter a run and a backgrounded `deep` stalls on
//! `SIGTTIN`). This lives in its own test binary because it replaces this process's fd 0, which
//! would leak into any test running alongside it.
#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn child_reading_stdin_gets_eof_instead_of_the_parent_stdin() {
    // Point our stdin at a pipe nobody ever writes to (the write end stays open), standing in
    // for an idle terminal. A child that inherited it would block in `cat` until the timeout.
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element buffer for pipe(2), and dup2 only replaces fd 0 of
    // this single-test process; the pipe's write end is deliberately left open.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        assert_eq!(libc::dup2(fds[0], 0), 0);
    }

    let start = Instant::now();
    let out = indexa_parsers::proc::run_capped(Command::new("cat"), Duration::from_secs(10))
        .expect("cat must see EOF on stdin and exit, not hang until the timeout");
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(start.elapsed() < Duration::from_secs(5));
}
