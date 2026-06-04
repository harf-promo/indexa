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
    store::{ChunkRecord, Store},
    watcher::{self, ChangeKind, WatcherConfig},
};
use serde::Serialize;
use std::path::{Path, PathBuf};
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

    let mut sessions = state.watch_sessions.lock().await;
    if sessions.contains_key(&path) {
        return Json(serde_json::json!({ "watching": true, "already_running": true }))
            .into_response();
    }

    let db_path = (*state.db_path).clone();
    let embedder = state.embedder.clone();
    let embed_model = state.config.embedding.model.clone();
    let max_parse_bytes = state.config.parsers.max_file_mb.saturating_mul(1024 * 1024);
    let watch_root = PathBuf::from(&path);
    let watch_root2 = watch_root.clone();
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
                                    let _ = store.mark_for_resummary(&d, "dir", depth);
                                }
                            }
                        }
                    }
                    ChangeKind::Upsert => {
                        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        let extracted = match indexa_parsers::registry::parse_guarded(
                            path,
                            size,
                            max_parse_bytes,
                        ) {
                            Ok(e) => e,
                            Err(_) => return,
                        };
                        if extracted.chunks.is_empty() {
                            return;
                        }

                        let chunk_records: Vec<ChunkRecord> = rt.block_on(async {
                            let mut records = Vec::with_capacity(extracted.chunks.len());
                            for chunk in &extracted.chunks {
                                let embedding = embedder.embed(&chunk.text).await.ok();
                                records.push(ChunkRecord {
                                    entry_path: path.to_string_lossy().into_owned(),
                                    seq: chunk.seq,
                                    heading: chunk.heading.clone(),
                                    text: chunk.text.clone(),
                                    language: chunk.language.clone(),
                                    embedding,
                                    embed_model: Some(embed_model.clone()),
                                });
                            }
                            records
                        });

                        if let Ok(mut store) = Store::open(&db_path) {
                            if store.upsert_chunks(&chunk_records).is_ok() {
                                let s = path.to_string_lossy().into_owned();
                                let depth = path_depth(&s);
                                let _ = store.mark_for_resummary(&s, "file", depth);
                                for dir in
                                    ancestor_dirs_to_root(path, std::slice::from_ref(&watch_root2))
                                {
                                    let d = dir.to_string_lossy().into_owned();
                                    let dd = path_depth(&d);
                                    let _ = store.mark_for_resummary(&d, "dir", dd);
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

// ── Helpers (mirror of cmd_watch) ───────────────────────────────────────────

fn path_depth(path: &str) -> i64 {
    path.chars().filter(|&c| c == '/' || c == '\\').count() as i64
}

fn ancestor_dirs_to_root(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(root) = roots.iter().find(|r| path.starts_with(r)) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut cur = path.parent();
    while let Some(d) = cur {
        if !d.starts_with(root) {
            break;
        }
        dirs.push(d.to_path_buf());
        if d == root.as_path() {
            break;
        }
        cur = d.parent();
    }
    dirs
}
