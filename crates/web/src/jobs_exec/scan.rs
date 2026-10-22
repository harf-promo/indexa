//! Web scans use a dedicated connection so filesystem work never holds the API store mutex.

use super::{finalize_failed, orphan_prune_message, scan_walk_config};
use crate::jobs::{push, JobEvent, JobHandle};
use crate::AppState;
use indexa_core::{
    decisions::detectors::expire_vanished_decisions,
    store::Store,
    walker::{walk, Entry, WalkConfig},
};
use std::{path::Path, sync::Arc};

pub(super) async fn run_scan_phase(
    state: &AppState,
    path: &str,
    handle: &Arc<JobHandle>,
) -> Option<Vec<Entry>> {
    let _permit = state.walk_semaphore.acquire().await.ok()?;
    let db_path = state.db_path.clone();
    let path = std::path::PathBuf::from(path);
    let walk_cfg = scan_walk_config(&state.config.scan);
    let max_open = state.config.review.max_open;
    let job = handle.clone();
    match tokio::task::spawn_blocking(move || {
        let mut store = match Store::open(&db_path) {
            Ok(store) => store,
            Err(e) => {
                finalize_failed(&job, "scan", &e);
                return None;
            }
        };
        scan_with_walk(&mut store, &path, &walk_cfg, max_open, &job, walk)
    })
    .await
    {
        Ok(entries) => entries,
        Err(e) => {
            finalize_failed(handle, "scan", &anyhow::anyhow!(e));
            None
        }
    }
}

fn scan_with_walk(
    store: &mut Store,
    path: &Path,
    walk_cfg: &WalkConfig,
    max_open: usize,
    handle: &Arc<JobHandle>,
    walk: impl FnOnce(&Path, &WalkConfig) -> anyhow::Result<Vec<Entry>>,
) -> Option<Vec<Entry>> {
    // Use the same index-wide lease as CLI scans. Acquire it before allocating the
    // generation or walking, and retain it through reconciliation and hygiene below.
    let _lease = match store.scan_lease() {
        Ok(lease) => lease,
        Err(e) => {
            finalize_failed(handle, "scan", &e);
            return None;
        }
    };
    let generation = match store.next_scan_generation() {
        Ok(generation) => generation,
        Err(e) => {
            finalize_failed(handle, "scan", &e);
            return None;
        }
    };
    let entries = match walk(path, walk_cfg) {
        Ok(entries) => entries,
        Err(e) => {
            finalize_failed(handle, "walk", &e);
            return None;
        }
    };
    // The walk is fail-open, so a missing/unreadable root yields nothing — and reconciling that
    // would delete everything indexed under it. Fail the job and leave the index untouched.
    if let Err(e) = indexa_core::walker::ensure_walked_root_present(path, entries.len()) {
        let e = e.context("scan skipped — this root's existing index was left untouched");
        finalize_failed(handle, "scan", &e);
        return None;
    }
    let n = entries.len() as u64;
    push(
        handle,
        JobEvent::Start {
            kind: "scan".into(),
            path: path.to_string_lossy().into_owned(),
            total: Some(n),
        },
    );

    if let Err(e) = store.upsert_entries_with_generation(&entries, Some(generation)) {
        finalize_failed(handle, "scan", &e);
        return None;
    }
    if let Err(e) = store.reconcile_by_generation(&path.to_string_lossy(), generation) {
        push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("{e:#}"),
                pressure: None,
            },
        );
    }
    // Self-heal: drop chunks/summaries left orphaned (no entry row) — e.g. build artifacts
    // indexed by an older version. `reconcile_by_generation` only cleans ghost *entries*, not
    // orphans, so without this the index can carry stale junk until a manual `indexa prune`.
    match store.prune_orphans() {
        Ok(orphans) if !orphans.is_empty() => push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: orphan_prune_message(&orphans),
                pressure: None,
            },
        ),
        Ok(_) => {}
        Err(e) => push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("orphan prune skipped: {e:#}"),
                pressure: None,
            },
        ),
    }
    // H1: expire open Review questions whose subject left the index. Only the CLI's
    // `indexa index`/`indexa review` ran this (via run_detectors) — a web-only "Build
    // context" workflow, or a subject later excluded by a `[scan] ignore` edit, could
    // leave a stale question in the inbox forever. Runs alongside prune_orphans above:
    // same "index hygiene after a scan" concern, same natural cadence.
    match expire_vanished_decisions(store, max_open) {
        Ok(n) if n > 0 => push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("expired {n} Review question(s) whose subject left the index"),
                pressure: None,
            },
        ),
        Ok(_) => {}
        Err(e) => push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("decision-expiry sweep skipped: {e:#}"),
                pressure: None,
            },
        ),
    }

    push(
        handle,
        JobEvent::Progress {
            current: n,
            total: n,
            note: Some(format!("{n} entries scanned")),
            current_path: None,
            items_per_sec: None,
            eta_secs: None,
        },
    );
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::walker::walk_streaming;
    use std::sync::{mpsc, Barrier};

    // The CLI's production protocol: one lease/generation before walking, retained through
    // streaming writes, reconciliation and orphan cleanup. No CLI environment/global DB paths.
    fn cli_scan(store: &mut Store, root: &Path) {
        let _lease = store.scan_lease().unwrap();
        let generation = store.next_scan_generation().unwrap();
        walk_streaming(root, &WalkConfig::default(), |batch| {
            store.upsert_entries_with_generation(&batch, Some(generation))
        })
        .unwrap();
        store
            .reconcile_by_generation(&root.to_string_lossy(), generation)
            .unwrap();
        store.prune_orphans().unwrap();
    }

    fn interleaved_scan(newer_is_web: bool) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("kept.md"), "kept").unwrap();
        let ghost = root.join("ghost.md");
        std::fs::write(&ghost, "removed before the next scan").unwrap();
        let db = dir.path().join("index.db");
        let mut seed = Store::open(&db).unwrap();
        cli_scan(&mut seed, &root);
        std::fs::remove_file(&ghost).unwrap();

        let snapshot_ready = Arc::new(Barrier::new(2));
        let finish_older = Arc::new(Barrier::new(2));
        let start_newer = Arc::new(Barrier::new(2));
        let (probe_tx, probe_rx) = mpsc::channel();
        let older = {
            let db = db.clone();
            let root = root.clone();
            let snapshot_ready = snapshot_ready.clone();
            let finish_older = finish_older.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                let handle = Arc::new(JobHandle::new("scan", root.to_string_lossy()));
                assert!(scan_with_walk(
                    &mut store,
                    &root,
                    &WalkConfig::default(),
                    50,
                    &handle,
                    |root, cfg| {
                        let entries = walk(root, cfg)?;
                        snapshot_ready.wait();
                        finish_older.wait();
                        Ok(entries)
                    },
                )
                .is_some());
            })
        };
        snapshot_ready.wait();
        // The older web scan has already collected its snapshot, which omits this new file.
        let newest = root.join("newest.md");
        std::fs::write(&newest, "written after the older walk").unwrap();
        let newer = {
            let db = db.clone();
            let root = root.clone();
            let start_newer = start_newer.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                let mut lock_path = std::fs::canonicalize(&db).unwrap().into_os_string();
                lock_path.push(".scan-lock");
                let probe =
                    rusqlite::Connection::open(std::path::PathBuf::from(lock_path)).unwrap();
                probe.busy_timeout(std::time::Duration::ZERO).unwrap();
                let busy = match probe.execute_batch("BEGIN IMMEDIATE") {
                    Err(rusqlite::Error::SqliteFailure(err, _))
                        if err.code == rusqlite::ErrorCode::DatabaseBusy =>
                    {
                        true
                    }
                    Ok(()) => false,
                    result => panic!("unexpected scan lease probe: {result:?}"),
                };
                drop(probe);
                probe_tx.send(busy).unwrap();
                if busy {
                    // Let the older scan reconcile first; this avoids timing/timeout assertions.
                    start_newer.wait();
                }
                if newer_is_web {
                    let handle = Arc::new(JobHandle::new("scan", root.to_string_lossy()));
                    assert!(scan_with_walk(
                        &mut store,
                        &root,
                        &WalkConfig::default(),
                        50,
                        &handle,
                        walk,
                    )
                    .is_some());
                } else {
                    cli_scan(&mut store, &root);
                }
            })
        };
        let lease_was_held = probe_rx.recv().unwrap();
        if lease_was_held {
            finish_older.wait();
            older.join().unwrap();
            start_newer.wait();
            newer.join().unwrap();
        } else {
            // Reproduce the original race deterministically: the newer scan commits before
            // releasing the older snapshot to reconcile. No scheduling assumptions or sleeps.
            newer.join().unwrap();
            finish_older.wait();
            older.join().unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert!(
            store
                .entry_by_path(&newest.to_string_lossy())
                .unwrap()
                .is_some(),
            "older web snapshot pruned a row written by the newer scan"
        );
        assert!(
            lease_was_held,
            "web must hold the CLI scan lease before walking"
        );
        assert!(store
            .entry_by_path(&ghost.to_string_lossy())
            .unwrap()
            .is_none());
        let generation: i64 = store
            .db_connection()
            .query_row(
                "SELECT scan_generation FROM entries WHERE path = ?1",
                [newest.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generation, 3, "both scans must allocate under the lease");
    }

    #[test]
    fn web_scan_interleaving_with_cli_preserves_newer_rows() {
        interleaved_scan(false);
    }

    #[test]
    fn web_scan_interleaving_with_web_preserves_newer_rows() {
        interleaved_scan(true);
    }
}
