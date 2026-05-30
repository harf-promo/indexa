use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{err_json, require_path, PathQuery, QueueFailedItem, QueueStats};
use crate::AppState;

pub(crate) async fn api_queue_stats(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    // Surface a DB fault as 500 rather than masking it as an empty/zero queue (which would
    // misreport a corrupt/locked index as "nothing queued").
    match store.queue_stats() {
        Ok(qs) => Json(QueueStats {
            pending: qs.pending as u64,
            in_flight: qs.in_flight as u64,
            done: qs.done as u64,
            failed: qs.failed as u64,
        })
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_queue_failed(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.failed_queue_items(50) {
        Ok(items) => Json(
            items
                .into_iter()
                .map(|i| QueueFailedItem {
                    path: i.path,
                    error: i.error,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_queue_retry(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut store = state.store.lock().await;
    match store.mark_queue_state(&path, "pending", None) {
        Ok(_) => Json(serde_json::json!({ "queued": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
