//! `GET /api/watch/status`, `POST /api/watch/start`, `POST /api/watch/stop`
//!
//! Embeds the `indexa watch` logic directly in `serve`, so users can start/stop
//! filesystem watching per root from the web UI without a separate terminal.
//! Each watch task re-embeds changed files and marks stale summaries for
//! re-generation — exactly as `indexa watch` does.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::{
    pathutil::{ancestor_dirs_to_root, path_depth},
    store::{chunk_content_hash, ChunkRecord, Store},
    walker::{Entry, EntryKind},
    watcher::{self, ChangeKind, WatcherConfig},
};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, atomic::AtomicU64, atomic::Ordering, Arc};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dto::{err_json, require_path, PathQuery};
use crate::handlers::path_policy::canonical_indexed_path;
use crate::{AppState, WatchTaskInfo};

#[derive(Serialize)]
pub(crate) struct WatchStatusEntry {
    pub(crate) path: String,
    pub(crate) watching: bool,
    pub(crate) events_count: u64,
    pub(crate) started_at: u64,
}

fn remove_session_if_current(
    sessions: &mut std::collections::HashMap<String, WatchTaskInfo>,
    path: &str,
    session_id: uuid::Uuid,
) -> bool {
    if sessions
        .get(path)
        .is_some_and(|info| info.session_id == session_id)
    {
        sessions.remove(path);
        true
    } else {
        false
    }
}

pub(crate) async fn api_watch_status(State(state): State<AppState>) -> Response {
    let sessions = state.watch_sessions.lock().await;
    let list: Vec<WatchStatusEntry> = sessions
        .iter()
        .map(|(path, info)| WatchStatusEntry {
            path: path.clone(),
            watching: true,
            events_count: info.events_count.load(Ordering::Relaxed),
            started_at: info.started_at,
        })
        .collect();
    Json(list).into_response()
}

/// Persist one re-parsed file: entry row, chunks, code graph, then re-queue its summary and
/// ancestor roll-ups. Split out of the watch closure (which needs a live notify debouncer) so
/// the persistence contract is testable.
fn persist_upsert(
    store: &mut Store,
    path: &std::path::Path,
    watch_root: &std::path::Path,
    entry: Entry,
    chunk_records: &[ChunkRecord],
    edges: &[indexa_parsers::types::Edge],
) {
    let s = path.to_string_lossy().into_owned();
    // A newly-created file has no `entries` row (only `scan` writes those), so without this
    // its chunks are orphans: never summarized and wiped by the next `prune`. Idempotent
    // upsert also refreshes size/mtime on edits.
    if let Err(e) = store.upsert_entries(&[entry]) {
        tracing::warn!(path = %s, error = %e, "watch: failed to upsert entry");
    }
    let chunks_ok = store.replace_file_chunks(&s, chunk_records).is_ok();
    // `deep` skips a file whose chunks are current, so a watcher that re-chunks without
    // refreshing the code graph leaves `symbols`/`edges` stale until the file changes again
    // with watch off. Refresh the graph in the same pass, as `deep` does.
    if let Err(e) = indexa_query::code_graph::refresh_file_code_graph(store, &s, edges) {
        tracing::warn!(path = %s, error = %e, "watch: failed to refresh code graph");
    }
    if chunks_ok {
        let depth = path_depth(&s);
        if let Err(e) = store.mark_for_resummary(&s, "file", depth) {
            tracing::warn!(path = %s, error = %e, "watch: failed to mark changed file for re-summary");
        }
        for dir in ancestor_dirs_to_root(path, std::slice::from_ref(&watch_root.to_path_buf())) {
            let d = dir.to_string_lossy().into_owned();
            let dd = path_depth(&d);
            if let Err(e) = store.mark_for_resummary(&d, "dir", dd) {
                tracing::warn!(dir = %d, error = %e, "watch: failed to mark ancestor dir for re-summary after upsert");
            }
        }
    }
}

pub(crate) async fn api_watch_start(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let requested = match require_path(params) {
        Ok(p) => p,
        Err(err) => return err.into_response(),
    };
    let validated_path = match canonical_indexed_path(&state, &requested).await {
        Ok(path) => path,
        Err(err) => return err.into_response(),
    };
    let path = validated_path.as_str().to_owned();

    let mut sessions = state.watch_sessions.lock().await;
    if sessions.contains_key(&path) {
        return Json(serde_json::json!({ "watching": true, "already_running": true }))
            .into_response();
    }

    let db_path = (*state.db_path).clone();
    let embedder = state.embedder.clone();
    let embed_model = state.config.embedding.model.clone();
    let max_parse_bytes = state.config.parsers.max_file_mb.saturating_mul(1024 * 1024);
    // Chunk-aware registry honoring `[chunking]` size/overlap; built before the `'static` watch
    // task so it can be moved in and reused for every event.
    let mut registry =
        indexa_parsers::registry::Registry::with_chunk(indexa_parsers::types::ChunkParams {
            size: state.config.chunking.size,
            overlap: state.config.chunking.overlap,
            encoding: indexa_parsers::types::TextEncoding::from_config_str(
                &state.config.parsers.encoding,
            ),
        });
    registry.register_preprocessors(&crate::preprocessor_specs(&state.config));
    if state.config.parsers.compressed {
        registry.enable_compressed();
    }
    let watch_root = PathBuf::from(&path);
    let watch_root2 = watch_root.clone();
    // Same per-event file-selection policy the scan walker uses (skip artifacts/sensitive/oversized/
    // ignored paths); built once and moved into the watch task.
    let scan_matchers = indexa_core::walker::build_scan_matchers(
        std::slice::from_ref(&watch_root),
        state.config.scan.respect_gitignore,
        &state.config.scan.ignore,
        state.config.scan.custom_ignore,
    );
    let include_sensitive = state.config.scan.include_sensitive;
    let redact_at_index = state.config.scan.redact_at_index;
    let events_count = Arc::new(AtomicU64::new(0));
    let events_count2 = events_count.clone();
    // Cooperative stop flag: `api_watch_stop` sets it to end the blocking watch loop (which
    // drops the debouncer). `stop2` is moved into the loop; `stop` is kept in `WatchTaskInfo`.
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();

    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let session_id = uuid::Uuid::new_v4();
    let task = tokio::spawn(async move {
        let session = match watcher::watch(&[&watch_root], &WatcherConfig::default()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "watch: could not start watcher for {}: {e}",
                    watch_root.display()
                );
                return;
            }
        };
        // notify accepts a pathname rather than a directory handle. Check the watched
        // directory identity after attachment so a swap between request validation and
        // watcher startup cannot silently attach the watch to an outside directory.
        if let Err(e) = validated_path.revalidate() {
            tracing::warn!(
                "watch: refusing changed root {} after watcher startup: {}",
                watch_root.display(),
                e.message
            );
            stop2.store(true, Ordering::Relaxed);
            return;
        }
        let rt = tokio::runtime::Handle::current();
        let validated_for_events = validated_path.clone();
        tokio::task::spawn_blocking(move || {
            watcher::run_watch_loop_until(session, &stop2, |event| {
                if let Err(e) = validated_for_events.revalidate() {
                    tracing::warn!(
                        "watch: stopping after root identity changed for {}: {}",
                        watch_root2.display(),
                        e.message
                    );
                    stop2.store(true, Ordering::Relaxed);
                    return;
                }
                let path = &event.path;
                // A directory upsert carries no content (the watcher follows it with one event per
                // file), but a directory *remove* must still clear its subtree: the watcher emits one
                // for a directory that a rename swapped out while the path exists again.
                if event.kind == ChangeKind::Upsert && path.is_dir() {
                    return;
                }
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.'))
                    .unwrap_or(false)
                {
                    return;
                }

                events_count2.fetch_add(1, Ordering::Relaxed);

                match event.kind {
                    ChangeKind::Remove => {
                        if let Ok(mut store) = Store::open(&db_path) {
                            let s = path.to_string_lossy().into_owned();
                            if let Err(e) = store.delete_subtree(&s) {
                                tracing::warn!("watch: remove {s}: {e}");
                            } else {
                                for dir in
                                    ancestor_dirs_to_root(path, std::slice::from_ref(&watch_root2))
                                {
                                    let d = dir.to_string_lossy().into_owned();
                                    let depth = path_depth(&d);
                                    if let Err(e) = store.mark_for_resummary(&d, "dir", depth) {
                                        tracing::warn!(dir = %d, error = %e, "watch: failed to mark ancestor dir for re-summary after remove");
                                    }
                                }
                            }
                        }
                    }
                    ChangeKind::Upsert => {
                        // Skip build artifacts / sensitive files / oversized blobs / ignored paths,
                        // matching the scan walker so a live watch doesn't pollute the index.
                        if !indexa_core::walker::should_index_file(
                            path,
                            std::slice::from_ref(&watch_root2),
                            include_sensitive,
                            Some(indexa_core::walker::DEFAULT_MAX_FILESIZE),
                            &scan_matchers,
                        ) {
                            return;
                        }
                        let meta = std::fs::metadata(path).ok();
                        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                        let extracted = match registry.parse_guarded(path, size, max_parse_bytes) {
                            Ok(e) => e,
                            Err(_) => return,
                        };

                        // `block_on` is safe here: this closure runs on a `spawn_blocking`
                        // thread (not an async executor thread), so driving the embed futures
                        // to completion blocks only this blocking-pool thread — never the
                        // runtime's async workers. The awaited embeds park on the IO driver
                        // running on those separate workers.
                        let chunk_records: Vec<ChunkRecord> = rt.block_on(async {
                            let mut records = Vec::with_capacity(extracted.chunks.len());
                            for chunk in &extracted.chunks {
                                let embedding = match embedder.embed(&chunk.text).await {
                                    Ok(e) => Some(e),
                                    Err(e) => {
                                        // The chunk is still stored (searchable via BM25), but
                                        // without a vector it won't match dense retrieval. Surface
                                        // it — a silently-unembedded chunk degrades search invisibly.
                                        tracing::warn!(
                                            path = %path.display(),
                                            seq = chunk.seq,
                                            error = %e,
                                            "watch: embedding failed; chunk stored without a vector"
                                        );
                                        None
                                    }
                                };
                                records.push(ChunkRecord {
                                    entry_path: path.to_string_lossy().into_owned(),
                                    seq: chunk.seq,
                                    heading: chunk.heading.clone(),
                                    text: indexa_query::redact::chunk_text_for_store(
                                        &chunk.text,
                                        redact_at_index,
                                    ),
                                    language: chunk.language.clone(),
                                    embedding,
                                    embed_model: Some(embed_model.clone()),
                                    content_hash: Some(chunk_content_hash(&chunk.text)),
                                });
                            }
                            records
                        });

                        if let Ok(mut store) = Store::open(&db_path) {
                            let entry = Entry {
                                path: path.to_path_buf(),
                                kind: EntryKind::File,
                                size,
                                modified: meta.as_ref().and_then(|m| m.modified().ok()),
                                hint: indexa_core::surface::classify(path).or_else(|| indexa_core::surface::classify_file_by_extension(path)),
                                is_binary: false,
                            };
                            persist_upsert(
                                &mut store,
                                path,
                                &watch_root2,
                                entry,
                                &chunk_records,
                                &extracted.edges,
                            );
                        }
                    }
                }
            });
        })
        .await
        .ok();
    });

    let abort = task.abort_handle();
    sessions.insert(
        path.clone(),
        WatchTaskInfo {
            session_id,
            abort,
            stop,
            events_count,
            started_at,
        },
    );

    // Watchdog: remove the session entry when the task finishes (normally, panics, or is
    // aborted). Without this, a crashed watcher leaves a zombie entry so the UI shows
    // "watching" forever even though no events are flowing.
    {
        let sessions_weak = state.watch_sessions.clone();
        let cleanup_path = path.clone();
        tokio::spawn(async move {
            let _ = task.await; // await completion/panic/abort
            let mut sessions = sessions_weak.lock().await;
            remove_session_if_current(&mut sessions, &cleanup_path, session_id);
            tracing::debug!("watch: session cleanup checked for {cleanup_path}");
        });
    }

    Json(serde_json::json!({ "watching": true, "path": path })).into_response()
}

pub(crate) async fn api_watch_stop(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let requested = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // Session keys use canonical paths; this also lets a caller stop the same watch through a
    // lexical `.`/`..` spelling. A removed root can still be stopped by its original path.
    let path = std::fs::canonicalize(&requested)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(requested);

    let mut sessions = state.watch_sessions.lock().await;
    if let Some(info) = sessions.remove(&path) {
        // Signal the blocking watch loop to exit: it drops the debouncer (stopping the OS
        // watcher) and stops opening a fresh `Store` per event. `abort` only nudges the outer
        // async wrapper so the watchdog cleanup fires promptly — it can't stop the loop itself.
        info.stop.store(true, Ordering::Relaxed);
        info.abort.abort();
        Json(serde_json::json!({ "stopped": true, "path": path })).into_response()
    } else {
        err_json(
            StatusCode::NOT_FOUND,
            format!("no active watch session for '{path}'"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn late_stop_cleanup_keeps_immediately_restarted_session() {
        fn info(session_id: uuid::Uuid) -> (WatchTaskInfo, tokio::task::JoinHandle<()>) {
            let task = tokio::spawn(std::future::pending::<()>());
            let info = WatchTaskInfo {
                session_id,
                abort: task.abort_handle(),
                stop: Arc::new(AtomicBool::new(false)),
                events_count: Arc::new(AtomicU64::new(0)),
                started_at: 0,
            };
            (info, task)
        }

        let path = "/indexed/root".to_owned();
        let old_id = uuid::Uuid::new_v4();
        let new_id = uuid::Uuid::new_v4();
        let (old, old_task) = info(old_id);
        let (new, new_task) = info(new_id);
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(path.clone(), old);

        // `stop` removes the old map entry, and the next `start` inserts the replacement before
        // the old watchdog necessarily wakes from its aborted wrapper.
        let stopped = sessions.remove(&path).unwrap();
        stopped.stop.store(true, Ordering::Relaxed);
        stopped.abort.abort();
        sessions.insert(path.clone(), new);

        assert!(!remove_session_if_current(&mut sessions, &path, old_id));
        assert_eq!(sessions.get(&path).unwrap().session_id, new_id);
        sessions.remove(&path).unwrap().abort.abort();
        old_task.abort();
        new_task.abort();
    }

    fn defines(name: &str, range: (usize, usize)) -> indexa_parsers::types::Edge {
        indexa_parsers::types::Edge {
            from: PathBuf::from("/r/a.rs"),
            kind: "defines",
            to: name.to_owned(),
            symbol_kind: Some("fn"),
            line_range: Some(range),
        }
    }

    /// H3: a watch upsert must refresh the code graph, not just the chunks. `deep` skips a
    /// file whose chunks are current, so without this the stale symbol range was permanent.
    #[test]
    fn watch_upsert_refreshes_the_code_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("a.rs");
        std::fs::write(&file, "fn run() {}\n").unwrap();
        let mut store = Store::open(&dir.path().join("idx.db")).unwrap();
        let entry = || Entry {
            path: file.clone(),
            kind: EntryKind::File,
            size: 12,
            modified: None,
            hint: None,
            is_binary: false,
        };
        let path_str = file.to_string_lossy().into_owned();

        persist_upsert(
            &mut store,
            &file,
            &root,
            entry(),
            &[],
            &[defines("run", (1, 3))],
        );
        let syms = store.symbols_in_file(&path_str).unwrap();
        assert_eq!((syms[0].start_line, syms[0].end_line), (1, 3));

        persist_upsert(
            &mut store,
            &file,
            &root,
            entry(),
            &[],
            &[defines("run", (10, 14)), defines("other", (16, 18))],
        );
        let mut syms = store.symbols_in_file(&path_str).unwrap();
        syms.sort_by_key(|s| s.start_line);
        assert_eq!(syms.len(), 2, "stale rows replaced, not appended: {syms:?}");
        assert_eq!((syms[0].name.as_str(), syms[0].start_line), ("run", 10));
        assert_eq!((syms[1].name.as_str(), syms[1].start_line), ("other", 16));
    }
}
