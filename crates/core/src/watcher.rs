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
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
    EventKind, RecursiveMode,
};
use notify_debouncer_full::{new_debouncer_opt, DebounceEventResult, NoCache};
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
    //
    // `NoCache` instead of the platform-recommended file-ID cache: on macOS FSEvents reports a
    // sticky `ItemCreated` flag on a recently created directory, so the cache-backed debouncer
    // stitched its rename as "created, then renamed" and moved the queue to the destination —
    // the source path got no event at all and its subtree stayed in the index. Without the
    // cache each side of the rename arrives on its own and `change_events` resolves direction by
    // existence. Linux already uses `NoCache` (inotify cookies still pair renames); it also
    // skips the cache's full recursive scan of every watched root.
    let mut debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, NoCache>(
        cfg.debounce,
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                let events = events.iter().flat_map(|ev| change_events(&ev.event));
                for event in expand_directory_upserts(events) {
                    debug!("fs event {:?}: {}", event.kind, event.path.display());
                    let _ = tx.send(event);
                }
            }
            Err(errors) => {
                for e in errors {
                    warn!("watch error: {e}");
                }
            }
        },
        NoCache,
        notify::Config::default(),
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

// Keep event translation separate from OS delivery so rename direction can be tested
// deterministically without relying on a filesystem backend's timing.
fn change_events(event: &notify::Event) -> Vec<ChangeEvent> {
    event
        .paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let kind = match &event.kind {
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => {
                    if index == 0 {
                        ChangeKind::Remove
                    } else {
                        ChangeKind::Upsert
                    }
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => ChangeKind::Remove,
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => ChangeKind::Upsert,
                EventKind::Modify(ModifyKind::Name(_)) => {
                    // Some backends cannot identify direction. Only confirmed absence is a
                    // deletion; an IO/permission error must not erase a live index entry.
                    if matches!(path.try_exists(), Ok(false)) {
                        ChangeKind::Remove
                    } else {
                        ChangeKind::Upsert
                    }
                }
                EventKind::Create(CreateKind::File)
                | EventKind::Create(CreateKind::Folder)
                | EventKind::Create(CreateKind::Any)
                | EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Any) => ChangeKind::Upsert,
                EventKind::Remove(RemoveKind::File)
                | EventKind::Remove(RemoveKind::Any)
                | EventKind::Remove(RemoveKind::Folder) => ChangeKind::Remove,
                _ => return None,
            };
            Some(ChangeEvent {
                path: path.clone(),
                kind,
            })
        })
        .collect()
}

/// Follow every directory `Upsert` with an `Upsert` for each file beneath it.
///
/// A rename or move-in of a directory is reported for the directory alone, never for its
/// children, and consumers skip directory events — so without this the files under a renamed
/// directory were removed from their old paths but never indexed at their new ones. The walk
/// prunes the same build-artifact / VCS directories the scanner skips and does not follow
/// symlinks; consumers still apply their per-file indexing policy to every emitted path.
fn expand_directory_upserts(events: impl IntoIterator<Item = ChangeEvent>) -> Vec<ChangeEvent> {
    let mut out = Vec::new();
    for event in events {
        let expand = event.kind == ChangeKind::Upsert
            && std::fs::symlink_metadata(&event.path).is_ok_and(|m| m.is_dir())
            && !crate::walker::is_skip_dir(&event.path);
        let dir = expand.then(|| event.path.clone());
        out.push(event);
        if let Some(dir) = dir {
            push_descendant_files(&dir, &mut out);
        }
    }
    out
}

fn push_descendant_files(dir: &Path, out: &mut Vec<ChangeEvent>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!("watch: could not list {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if !crate::walker::is_skip_dir(&path) {
                push_descendant_files(&path, out);
            }
        } else if file_type.is_file() {
            out.push(ChangeEvent {
                path,
                kind: ChangeKind::Upsert,
            });
        }
    }
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

#[cfg(test)]
mod rename_tests {
    use super::*;
    use notify::event::RenameMode;

    fn events(mode: RenameMode, paths: &[&str]) -> Vec<(PathBuf, ChangeKind)> {
        let mut event = notify::Event::new(EventKind::Modify(ModifyKind::Name(mode)));
        event.paths = paths.iter().map(PathBuf::from).collect();
        change_events(&event)
            .into_iter()
            .map(|e| (e.path, e.kind))
            .collect()
    }

    #[test]
    fn paired_rename_removes_source_before_upserting_destination() {
        assert_eq!(
            events(RenameMode::Both, &["/old", "/new"]),
            vec![
                ("/old".into(), ChangeKind::Remove),
                ("/new".into(), ChangeKind::Upsert)
            ]
        );
    }

    #[test]
    fn split_rename_from_removes_and_to_upserts() {
        assert_eq!(
            events(RenameMode::From, &["/old"]),
            vec![("/old".into(), ChangeKind::Remove)]
        );
        assert_eq!(
            events(RenameMode::To, &["/new"]),
            vec![("/new".into(), ChangeKind::Upsert)]
        );
    }

    #[test]
    fn directory_upsert_expands_to_descendant_files_pruning_skip_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let moved = dir.path().join("moved");
        std::fs::create_dir_all(moved.join("sub")).unwrap();
        std::fs::create_dir_all(moved.join(".git")).unwrap();
        std::fs::write(moved.join("a.txt"), "a").unwrap();
        std::fs::write(moved.join("sub/b.txt"), "b").unwrap();
        std::fs::write(moved.join(".git/HEAD"), "ref").unwrap();
        let gone = dir.path().join("gone");

        let out = expand_directory_upserts([
            ChangeEvent {
                path: gone.clone(),
                kind: ChangeKind::Remove,
            },
            ChangeEvent {
                path: moved.clone(),
                kind: ChangeKind::Upsert,
            },
        ]);
        let mut got: Vec<_> = out.into_iter().map(|e| (e.path, e.kind)).collect();
        // The original events keep their order; descendants follow their directory.
        assert_eq!(got[0], (gone, ChangeKind::Remove));
        assert_eq!(got[1], (moved.clone(), ChangeKind::Upsert));
        got.drain(..2);
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                (moved.join("a.txt"), ChangeKind::Upsert),
                (moved.join("sub").join("b.txt"), ChangeKind::Upsert),
            ],
            "every file under the directory, but nothing under .git"
        );
    }

    #[test]
    fn ambiguous_rename_checks_whether_the_path_survived() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.txt");
        let new = dir.path().join("new.txt");
        std::fs::write(&new, "renamed").unwrap();
        assert_eq!(
            events(
                RenameMode::Any,
                &[old.to_str().unwrap(), new.to_str().unwrap()]
            ),
            vec![(old, ChangeKind::Remove), (new, ChangeKind::Upsert)]
        );
    }
}

/// Real-filesystem regression for a directory rename. Unlike `rename_tests`, this drives the
/// OS backend (FSEvents / inotify / ReadDirectoryChangesW) through [`watch`], so it proves the
/// event stream a consumer actually receives is enough to converge the index.
///
/// It applies each event the way both consumers (`indexa watch` and the web watch handler) do —
/// `Remove` → `delete_subtree`, a file `Upsert` → `upsert_entries` + `replace_file_chunks`, and
/// a directory `Upsert` is dropped — rather than invoking them, because they need an embedder
/// and parser registry. The fix lives in the event stream, so that is the level under test.
/// Only the final store state is asserted: backends differ in event kinds and order.
#[cfg(test)]
mod fs_rename_tests {
    use super::*;
    use crate::store::{ChunkRecord, Store};
    use crate::walker::{Entry, EntryKind};
    use std::time::Instant;

    fn canonical_root(p: &Path) -> PathBuf {
        // notify reports FSEvents paths canonicalized (`/var` → `/private/var`); on Windows
        // `canonicalize` adds a `\\?\` prefix that the event paths built from the root lack.
        let c = std::fs::canonicalize(p).unwrap();
        #[cfg(windows)]
        {
            if let Some(s) = c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
                return PathBuf::from(s);
            }
        }
        c
    }

    fn index_file(store: &mut Store, path: &Path) {
        let p = path.to_string_lossy().into_owned();
        store
            .upsert_entries(&[Entry {
                path: path.to_path_buf(),
                kind: EntryKind::File,
                size: 1,
                modified: None,
                hint: None,
                is_binary: false,
            }])
            .unwrap();
        store
            .replace_file_chunks(
                &p,
                &[ChunkRecord {
                    entry_path: p.clone(),
                    seq: 0,
                    heading: String::new(),
                    text: format!("content of {p}"),
                    language: None,
                    embedding: None,
                    embed_model: None,
                    content_hash: None,
                }],
            )
            .unwrap();
    }

    fn apply(store: &mut Store, event: &ChangeEvent) {
        if event.path.is_dir() {
            return;
        }
        match event.kind {
            ChangeKind::Remove => {
                store.delete_subtree(&event.path.to_string_lossy()).unwrap();
            }
            ChangeKind::Upsert => {
                if event.path.is_file() {
                    index_file(store, &event.path);
                }
            }
        }
    }

    fn indexed(store: &Store, path: &Path) -> bool {
        let p = path.to_string_lossy();
        store.entry_exists(&p).unwrap() && store.first_chunk_text(&p).unwrap().is_some()
    }

    #[test]
    fn renaming_a_directory_reindexes_its_descendants_at_the_new_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical_root(tmp.path());
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(old.join("sub")).unwrap();
        let rel = ["a.txt", "sub/b.txt"];
        for r in rel {
            std::fs::write(old.join(r), format!("body of {r}")).unwrap();
        }

        // The index lives outside the watched root so its own writes don't add events.
        let db_dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_dir.path().join("index.db")).unwrap();
        for r in rel {
            index_file(&mut store, &old.join(r));
        }

        let session = watch(
            &[&root],
            &WatcherConfig {
                debounce: Duration::from_millis(150),
            },
        )
        .unwrap();
        // Wait until the backend is armed (FSEvents has a startup latency of ~1s): touch a
        // probe until its event arrives, then drain, so the rename is never lost to arming.
        let probe = root.join("probe.txt");
        let arm_deadline = Instant::now() + Duration::from_secs(10);
        let mut armed = false;
        while !armed && Instant::now() < arm_deadline {
            std::fs::write(&probe, "probe").unwrap();
            while let Ok(event) = session.rx.recv_timeout(Duration::from_millis(300)) {
                armed |= event.path == probe;
            }
        }
        assert!(armed, "watcher never delivered the probe event");
        std::fs::rename(&old, &new).unwrap();

        let converged = |store: &Store| {
            rel.iter()
                .all(|r| !indexed(store, &old.join(r)) && indexed(store, &new.join(r)))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while !converged(&store) && Instant::now() < deadline {
            if let Ok(event) = session.rx.recv_timeout(Duration::from_millis(200)) {
                apply(&mut store, &event);
                seen.push(event);
            }
        }

        let state: Vec<_> = rel
            .iter()
            .map(|r| {
                (
                    *r,
                    indexed(&store, &old.join(r)),
                    indexed(&store, &new.join(r)),
                )
            })
            .collect();
        assert!(
            converged(&store),
            "index did not converge after renaming {} → {}; \
             (file, still indexed at old path, indexed at new path) = {state:?}; events = {seen:?}",
            old.display(),
            new.display()
        );
    }
}
