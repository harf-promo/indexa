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
use std::sync::{atomic::AtomicU64, atomic::Ordering, Arc};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dto::{err_json, require_path, PathQuery};
use crate::{AppState, WatchTaskInfo};

#[derive(Serialize)]
pub(crate) struct WatchStatusEntry {
    pub(crate) path: String,
    pub(crate) watching: bool,
    pub(crate) events_count: u64,
    pub(crate) started_at: u64,
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

pub(crate) async fn api_watch_start(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    // Reject absurdly long paths before they hit the filesystem watcher. 4096 is the common
    // PATH_MAX; a real watch root is far shorter, so anything past this is malformed input.
    if path.len() > 4096 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "watch path is too long (max 4096 bytes)",
        );
    }

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
    let registry =
        indexa_parsers::registry::Registry::with_chunk(indexa_parsers::types::ChunkParams {
            size: state.config.chunking.size,
            overlap: state.config.chunking.overlap,
        });
    let watch_root = PathBuf::from(&path);
    let watch_root2 = watch_root.clone();
    // Same per-event file-selection policy the scan walker uses (skip artifacts/sensitive/oversized/
    // ignored paths); built once and moved into the watch task.
    let scan_matchers = indexa_core::walker::build_scan_matchers(
        std::slice::from_ref(&watch_root),
        state.config.scan.respect_gitignore,
        &state.config.scan.ignore,
    );
    let include_sensitive = state.config.scan.include_sensitive;
    let redact_at_index = state.config.scan.redact_at_index;
    let events_count = Arc::new(AtomicU64::new(0));
    let events_count2 = events_count.clone();

    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

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
        let rt = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            watcher::run_watch_loop(session, |event| {
                let path = &event.path;
                if path.is_dir() {
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
                            if let Err(e) = store.delete_entry(&s) {
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
                        if extracted.chunks.is_empty() {
                            return;
                        }

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
                            // A newly-created file has no `entries` row (only `scan` writes those),
                            // so without this its chunks are orphans: never summarized and wiped by
                            // the next `prune`. Idempotent upsert also refreshes size/mtime on edits.
                            let entry = Entry {
                                path: path.to_path_buf(),
                                kind: EntryKind::File,
                                size,
                                modified: meta.as_ref().and_then(|m| m.modified().ok()),
                                hint: indexa_core::surface::classify(path).or_else(|| indexa_core::surface::classify_file_by_extension(path)),
                                is_binary: false,
                            };
                            if let Err(e) = store.upsert_entries(&[entry]) {
                                tracing::warn!(path = %path.display(), error = %e, "watch: failed to upsert entry");
                            }
                            if store.upsert_chunks(&chunk_records).is_ok() {
                                let s = path.to_string_lossy().into_owned();
                                let depth = path_depth(&s);
                                if let Err(e) = store.mark_for_resummary(&s, "file", depth) {
                                    tracing::warn!(path = %s, error = %e, "watch: failed to mark changed file for re-summary");
                                }
                                for dir in
                                    ancestor_dirs_to_root(path, std::slice::from_ref(&watch_root2))
                                {
                                    let d = dir.to_string_lossy().into_owned();
                                    let dd = path_depth(&d);
                                    if let Err(e) = store.mark_for_resummary(&d, "dir", dd) {
                                        tracing::warn!(dir = %d, error = %e, "watch: failed to mark ancestor dir for re-summary after upsert");
                                    }
                                }
                            }
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
            abort,
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
            sessions_weak.lock().await.remove(&cleanup_path);
            tracing::debug!("watch: session cleaned up for {cleanup_path}");
        });
    }

    Json(serde_json::json!({ "watching": true, "path": path })).into_response()
}

pub(crate) async fn api_watch_stop(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let mut sessions = state.watch_sessions.lock().await;
    if let Some(info) = sessions.remove(&path) {
        info.abort.abort();
        Json(serde_json::json!({ "stopped": true, "path": path })).into_response()
    } else {
        err_json(
            StatusCode::NOT_FOUND,
            format!("no active watch session for '{path}'"),
        )
    }
}
