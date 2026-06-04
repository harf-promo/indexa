//! Insights REST API (v0.10).
//!
//! Routes:
//!   GET /api/insights/duplicates?threshold=0.95&exact=false
//!   GET /api/insights/stale?days=365
//!   GET /api/insights/diff?days=7

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dto::err_json;
use crate::AppState;

#[derive(Deserialize)]
pub(crate) struct DuplicatesQuery {
    threshold: Option<f32>,
    exact: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct StaleQuery {
    days: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct DiffQuery {
    days: Option<i64>,
}

pub(crate) async fn api_insights_duplicates(
    State(state): State<AppState>,
    Query(q): Query<DuplicatesQuery>,
) -> Response {
    let threshold = q.threshold.unwrap_or(0.95).clamp(0.0, 1.0);
    let exact = q.exact.unwrap_or(false);

    let store = state.store.lock().await;
    let result = if exact {
        store.find_exact_duplicates()
    } else {
        store.find_near_duplicates(threshold)
    };

    match result {
        Ok(clusters) => {
            let items: Vec<serde_json::Value> = clusters
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "paths": c.paths,
                        "similarity": c.similarity,
                        "exact": c.exact,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "threshold": threshold,
                "exact": exact,
                "clusters": items,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_insights_stale(
    State(state): State<AppState>,
    Query(q): Query<StaleQuery>,
) -> Response {
    let days = q.days.unwrap_or(365).max(1);
    let store = state.store.lock().await;
    match store.find_stale_entries(days) {
        Ok(entries) => {
            let items: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "path": e.path,
                        "kind": e.kind,
                        "modified_s": e.modified_s,
                        "days_since_modified": e.days_since_modified,
                    })
                })
                .collect();
            Json(serde_json::json!({ "days": days, "entries": items })).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_insights_diff(
    State(state): State<AppState>,
    Query(q): Query<DiffQuery>,
) -> Response {
    let days = q.days.unwrap_or(7).max(1);
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - days * 86_400)
        .unwrap_or(0);

    let store = state.store.lock().await;
    match store.weekly_diff(since) {
        Ok(diff) => Json(serde_json::json!({
            "days": days,
            "added": diff.added,
            "modified": diff.modified,
            "added_count": diff.added_count,
            "modified_count": diff.modified_count,
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
