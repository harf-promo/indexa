use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::dto::{err_json, require_path, PathQuery};
use crate::AppState;

pub(crate) async fn api_delete_entry(
    Query(q): Query<PathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let path = match require_path(q) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // require_path accepts an empty string; guard the one destructive endpoint so an empty
    // (or whitespace) path can't reach delete_subtree.
    if path.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "path must not be empty");
    }
    let mut store = s.store.lock().await;
    match store.delete_subtree(&path) {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_version() -> impl IntoResponse {
    Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }))
}

/// Return the last N lines of today's log file (for error reports).
pub(crate) async fn api_logs_tail(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let lines: usize = params
        .get("lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(500);

    // tracing-appender rolling::daily creates files named "prefix.YYYY-MM-DD".
    // Pick the most recently modified log file under the log dir.
    let log_dir = &*state.log_dir;
    let candidates: Vec<_> = std::fs::read_dir(log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("indexa.log"))
        .collect();

    // Pick the most recently modified log file.
    let best = candidates
        .iter()
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    let content = match best {
        Some(entry) => std::fs::read_to_string(entry.path()).unwrap_or_default(),
        None => String::new(),
    };

    let tail: String = content
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    Json(serde_json::json!({ "lines": tail }))
}
