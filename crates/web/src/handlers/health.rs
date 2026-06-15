//! `GET /api/health` — server version + index freshness (v0.39).
//!
//! Surfaces the two things that silently rotted before: the running binary's
//! version (so a stale CLI/MCP/app is visible) and how long ago the index was
//! last updated (so answers built on a stale snapshot are flagged). No network,
//! no secrets — a cheap read the UI polls on load to show a staleness banner.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};

use crate::AppState;

/// Index is considered stale once its newest content is older than this. A week
/// is conservative: long enough not to nag during active work, short enough that
/// "answers may be out of date" is honest.
const STALE_AFTER_DAYS: i64 = 7;

pub(crate) async fn api_health(State(state): State<AppState>) -> Response {
    let (entries, chunks, last) = {
        let store = state.store.lock().await;
        (
            store.entry_count().unwrap_or(0),
            store.chunk_count().unwrap_or(0),
            store.last_indexed_at().ok().flatten(),
        )
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_days = last.map(|ts| ((now - ts) / 86_400).max(0));
    let stale = age_days.is_some_and(|d| d >= STALE_AFTER_DAYS);
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "entries": entries,
        "chunks": chunks,
        "index_age_days": age_days,
        "stale": stale,
    }))
    .into_response()
}
