//! Filesystem watcher — watches one or more roots for changes and re-indexes
//! modified files via a user-supplied callback.
//!
//! Uses `notify-debouncer-full`, which coalesces rapid bursts of events (editor
//! save dances: write-temp → rename → modify) into one batch on **all** platforms.
//! The plain `notify` poll-interval only debounced the fallback `PollWatcher`,
//! so on macOS (FSEvents) and Linux (inotify) a single save fired several events
//! and triggered redundant re-parse/re-embed of the same file.

use anyhow::{Context, Result};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    EventKind, RecursiveMode,
};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tracing::{debug, info, warn};

/// What happened to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// File or directory was created or modified.
    Upsert,
    /// File or directory was deleted.
    Remove,
}

/// A single change event emitted by the watcher.
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// Options for the watcher.
pub struct WatcherConfig {
    /// How long to wait after the last event before emitting (debounce window).
    pub debounce: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(500),
        }
    }
}

/// A running watcher session. The watcher continues until this is dropped.
pub struct WatchSession {
    // Boxed because the debouncer type is large and parameterised; we only need
    // to keep it alive for the duration of the session.
    _debouncer: Box<dyn std::any::Any + Send>,
    pub rx: mpsc::Receiver<ChangeEvent>,
}

/// Start watching `roots` for changes. Returns a `WatchSession` whose `rx`
/// channel receives one `ChangeEvent` per changed path (coalesced).
///
/// # Example
/// ```no_run
/// use indexa_core::watcher::{watch, WatcherConfig};
/// let session = watch(&["/tmp"], &WatcherConfig::default()).unwrap();
/// for event in session.rx {
///     println!("{:?} {:?}", event.kind, event.path);
/// }
/// ```
pub fn watch<P: AsRef<Path>>(roots: &[P], cfg: &WatcherConfig) -> Result<WatchSession> {
    let (tx, rx) = mpsc::channel::<ChangeEvent>();

    // The debouncer batches all events that arrive within `debounce` of each other
    // and delivers them once the burst settles — coalescing editor save dances.
    let mut debouncer =
        new_debouncer(
            cfg.debounce,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    for ev in events {
                        let kind = match &ev.event.kind {
                            EventKind::Create(CreateKind::File)
                            | EventKind::Create(CreateKind::Any)
                            | EventKind::Modify(ModifyKind::Data(_))
                            | EventKind::Modify(ModifyKind::Any)
                            | EventKind::Modify(ModifyKind::Name(_)) => ChangeKind::Upsert,
                            EventKind::Remove(RemoveKind::File)
                            | EventKind::Remove(RemoveKind::Any)
                            | EventKind::Remove(RemoveKind::Folder) => ChangeKind::Remove,
                            _ => continue, // ignore access, metadata-only, etc.
                        };
                        for path in &ev.event.paths {
                            debug!("fs event {:?}: {}", kind, path.display());
                            let _ = tx.send(ChangeEvent {
                                path: path.clone(),
                                kind: kind.clone(),
                            });
                        }
                    }
                }
                Err(errors) => {
                    for e in errors {
                        warn!("watch error: {e}");
                    }
                }
            },
        )
        .context("creating debounced filesystem watcher")?;

    for root in roots {
        let root = root.as_ref();
        debouncer
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;
        info!("watching {} for changes", root.display());
    }

    Ok(WatchSession {
        _debouncer: Box::new(debouncer),
        rx,
    })
}

/// How often the cooperative-stop loop wakes to re-check its `stop` flag while idle.
/// Small enough that a stop request is honored promptly; large enough that an idle
/// watcher isn't busy-spinning.
const STOP_POLL: Duration = Duration::from_millis(500);

/// Consume events from `session.rx` and call `on_change` for each.
/// Blocks the calling thread until the watcher is dropped or the channel closes.
/// Pass this to a blocking thread (or `tokio::task::spawn_blocking`) from async code.
pub fn run_watch_loop<F>(session: WatchSession, on_change: F)
where
    F: FnMut(ChangeEvent),
{
    // Delegate with a flag that is never set — behaviourally identical to the historical
    // blocking `for event in session.rx` loop. The CLI uses this and stops by process exit.
    let never = AtomicBool::new(false);
    run_watch_loop_until(session, &never, on_change);
}

/// Like [`run_watch_loop`], but returns once `stop` is observed `true` (checked after each
/// event and on every [`STOP_POLL`] idle tick). This lets a long-lived host (the web server)
/// terminate ONE watch without killing the process: setting `stop` ends the loop, which drops
/// `session` — and with it the debouncer — on *this* thread, stopping the OS watcher and
/// freeing every per-event resource. It exists because aborting the async task that spawned a
/// `spawn_blocking` closure cannot cancel that closure, so the debouncer would otherwise leak
/// and the loop would run forever.
pub fn run_watch_loop_until<F>(session: WatchSession, stop: &AtomicBool, on_change: F)
where
    F: FnMut(ChangeEvent),
{
    drain_until(&session.rx, stop, STOP_POLL, on_change);
    // `session` (incl. `_debouncer`) drops here → watcher threads stop, channel closes.
}

/// Core drain loop, decoupled from `WatchSession` so it can be unit-tested with a bare
/// `mpsc::channel` (no real debouncer to construct). Returns when `stop` is observed `true`,
/// or when the sender side disconnects.
fn drain_until<F>(
    rx: &mpsc::Receiver<ChangeEvent>,
    stop: &AtomicBool,
    poll: Duration,
    mut on_change: F,
) where
    F: FnMut(ChangeEvent),
{
    loop {
        match rx.recv_timeout(poll) {
            Ok(event) => on_change(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn watch_nonexistent_root_errors() {
        let result = watch(
            &["/tmp/indexa-does-not-exist-at-all-xyz"],
            &WatcherConfig::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn watch_existing_root_starts() {
        let dir = tempfile::tempdir().unwrap();
        let session = watch(&[dir.path()], &WatcherConfig::default()).unwrap();
        // Write a file — should produce at least one event within 1s.
        std::fs::write(dir.path().join("test.txt"), b"hello").unwrap();
        let event = session.rx.recv_timeout(Duration::from_secs(1));
        // Some CI environments may not deliver events, so we just check no panic.
        let _ = event;
    }

    fn ev(p: &str) -> ChangeEvent {
        ChangeEvent {
            path: PathBuf::from(p),
            kind: ChangeKind::Upsert,
        }
    }

    #[test]
    fn drain_until_processes_queued_events_then_exits_on_disconnect() {
        let (tx, rx) = mpsc::channel::<ChangeEvent>();
        for i in 0..3 {
            tx.send(ev(&format!("/f{i}"))).unwrap();
        }
        drop(tx); // disconnect after queuing → loop should drain then return
        let stop = AtomicBool::new(false);
        let mut seen = 0;
        drain_until(&rx, &stop, Duration::from_millis(5), |_| seen += 1);
        assert_eq!(seen, 3, "all queued events drained before disconnect exit");
    }

    #[test]
    fn drain_until_returns_promptly_when_stop_is_set() {
        // Sender stays alive (channel never disconnects); an idle watcher must still exit
        // because `stop` is set. This is the web watch-stop path — without the flag the
        // loop would block forever on `recv`.
        let (_tx, rx) = mpsc::channel::<ChangeEvent>();
        let stop = AtomicBool::new(true); // stop already requested
        let mut seen = 0;
        drain_until(&rx, &stop, Duration::from_millis(5), |_| seen += 1);
        assert_eq!(seen, 0, "no events, stop honored on the first idle tick");
    }

    #[test]
    fn drain_until_stops_after_processing_when_flag_flips_mid_stream() {
        // An event that arrives before the stop flag is still processed; the loop then
        // observes `stop` and returns without waiting for a disconnect.
        let (tx, rx) = mpsc::channel::<ChangeEvent>();
        tx.send(ev("/only")).unwrap();
        let stop = AtomicBool::new(true);
        let mut seen = 0;
        drain_until(&rx, &stop, Duration::from_millis(5), |_| seen += 1);
        assert_eq!(
            seen, 1,
            "the buffered event is handled, then stop ends the loop"
        );
        drop(tx);
    }
}
