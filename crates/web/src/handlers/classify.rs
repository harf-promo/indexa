//! `GET /api/classifications` + confirm/ignore endpoints.
//! Exposes the Smart-classification store to the web UI so users can review,
//! confirm, or dismiss the Tier-0 auto-detected category suggestions.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::dto::err_json;
use crate::AppState;

#[derive(Deserialize)]
pub(crate) struct ClassificationsQuery {
    /// Filter to one specific path; omit to list all auto suggestions.
    path: Option<String>,
    /// Source filter: "auto", "user", or "ignored". Omit for all.
    source: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ClassificationDto {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) category: String,
    pub(crate) confidence: f32,
    pub(crate) source: String,
}

#[derive(Deserialize)]
pub(crate) struct ConfirmRequest {
    pub(crate) path: String,
    pub(crate) category: String,
}

#[derive(Deserialize)]
pub(crate) struct IgnoreRequest {
    pub(crate) path: String,
}

/// List classifications for a specific path or all paths (filtered by source).
pub(crate) async fn api_classifications_list(
    State(state): State<AppState>,
    Query(params): Query<ClassificationsQuery>,
) -> Response {
    let store = state.store.lock().await;

    if let Some(path) = &params.path {
        // Single-path lookup
        match store.classification_for(path) {
            Ok(Some(rec)) => Json(vec![ClassificationDto {
                path: rec.path,
                kind: rec.kind,
                category: rec.category,
                confidence: rec.confidence,
                source: rec.source,
            }])
            .into_response(),
            Ok(None) => Json(Vec::<ClassificationDto>::new()).into_response(),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    } else {
        // List all, optionally by source
        let src = params.source.as_deref();
        match store.list_classifications(src, 500) {
            Ok(recs) => Json(
                recs.into_iter()
                    .map(|r| ClassificationDto {
                        path: r.path,
                        kind: r.kind,
                        category: r.category,
                        confidence: r.confidence,
                        source: r.source,
                    })
                    .collect::<Vec<_>>(),
            )
            .into_response(),
            Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }
}

/// Confirm (or correct) a classification — sets source='user'.
pub(crate) async fn api_classifications_confirm(
    State(state): State<AppState>,
    Json(body): Json<ConfirmRequest>,
) -> Response {
    let store = state.store.lock().await;
    match store.confirm_classification(&body.path, &body.category) {
        Ok(()) => Json(serde_json::json!({ "confirmed": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Ignore a classification — sets a sticky 'ignored' tombstone.
pub(crate) async fn api_classifications_ignore(
    State(state): State<AppState>,
    Json(body): Json<IgnoreRequest>,
) -> Response {
    let store = state.store.lock().await;
    match store.ignore_classification(&body.path) {
        Ok(()) => Json(serde_json::json!({ "ignored": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Reset (undo) a classification — deletes the row entirely so the path reverts to
/// "no suggestion". Re-running `indexa classify` will re-surface the auto suggestion.
pub(crate) async fn api_classifications_reset(
    State(state): State<AppState>,
    Json(body): Json<IgnoreRequest>, // only needs `path`
) -> Response {
    let mut store = state.store.lock().await;
    match store.delete_classification(&body.path) {
        Ok(()) => Json(serde_json::json!({ "reset": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
