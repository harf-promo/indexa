//! Saved searches REST API.
//!
//! Routes:
//!   GET    /api/saved        — list saved queries
//!   POST   /api/saved        — create/replace { name, question, mode?, scope? }
//!   DELETE /api/saved/:name  — delete by name

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::dto::err_json;
use crate::AppState;

#[derive(Serialize)]
struct SavedQueryDto {
    name: String,
    question: String,
    mode: String,
    scope: Option<String>,
    created_at: i64,
}

#[derive(Deserialize)]
pub(crate) struct SaveQueryBody {
    name: String,
    question: String,
    mode: Option<String>,
    scope: Option<String>,
}

pub(crate) async fn api_saved_list(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.list_saved_queries() {
        Ok(queries) => Json(
            queries
                .into_iter()
                .map(|q| SavedQueryDto {
                    name: q.name,
                    question: q.question,
                    mode: q.mode,
                    scope: q.scope,
                    created_at: q.created_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_saved_set(
    State(state): State<AppState>,
    Json(body): Json<SaveQueryBody>,
) -> Response {
    let name = body.name.trim();
    if name.is_empty() || body.question.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "name and question are required");
    }
    let mode = body.mode.as_deref().unwrap_or("rrf");
    if !["rrf", "sparse", "dense", "agentic"].contains(&mode) {
        return err_json(
            StatusCode::BAD_REQUEST,
            "mode must be 'rrf', 'sparse', 'dense', or 'agentic'",
        );
    }
    let mut store = state.store.lock().await;
    match store.save_query(name, body.question.trim(), mode, body.scope.as_deref()) {
        Ok(()) => Json(serde_json::json!({ "saved": true, "name": name })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_saved_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let mut store = state.store.lock().await;
    match store.delete_saved_query(&name) {
        Ok(0) => err_json(StatusCode::NOT_FOUND, format!("no saved query '{name}'")),
        Ok(_) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
