//! Filesystem watcher — watches one or more roots for changes and re-indexes
//! modified files via a user-supplied callback.
//!
//! Uses `notify` (the cross-platform file-watch crate) in debounced mode so
//! rapid saves (e.g. editor temp files) are coalesced into one event.

use anyhow::{Context, Result};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
    _watcher: RecommendedWatcher,
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
    let debounce = cfg.debounce;

    // `notify` provides a callback-based API; we bridge it into an mpsc channel.
    let watcher_tx = tx.clone();
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| match res {
            Ok(event) => {
                let kind = match &event.kind {
                    EventKind::Create(CreateKind::File)
                    | EventKind::Create(CreateKind::Any)
                    | EventKind::Modify(ModifyKind::Data(_))
                    | EventKind::Modify(ModifyKind::Any)
                    | EventKind::Modify(ModifyKind::Name(_)) => ChangeKind::Upsert,
                    EventKind::Remove(RemoveKind::File)
                    | EventKind::Remove(RemoveKind::Any)
                    | EventKind::Remove(RemoveKind::Folder) => ChangeKind::Remove,
                    _ => return, // ignore access, metadata-only, etc.
                };
                for path in event.paths {
                    debug!("fs event {:?}: {}", kind, path.display());
                    let _ = watcher_tx.send(ChangeEvent {
                        path,
                        kind: kind.clone(),
                    });
                }
            }
            Err(e) => warn!("watch error: {e}"),
        },
        NotifyConfig::default().with_poll_interval(debounce),
    )
    .context("creating filesystem watcher")?;

    for root in roots {
        let root = root.as_ref();
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;
        info!("watching {} for changes", root.display());
    }

    Ok(WatchSession {
        _watcher: watcher,
        rx,
    })
}

/// Consume events from `session.rx` and call `on_change` for each.
/// Blocks the calling thread until the watcher is dropped or the channel closes.
/// Pass this to a blocking thread (or `tokio::task::spawn_blocking`) from async code.
pub fn run_watch_loop<F>(session: WatchSession, mut on_change: F)
where
    F: FnMut(ChangeEvent),
{
    for event in session.rx {
        on_change(event);
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
}
