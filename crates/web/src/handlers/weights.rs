//! Importance weights REST API (v0.8).
//!
//! Routes:
//!   GET    /api/weights           — list all weights { kind? }
//!   POST   /api/weights           — set a weight { kind, target, weight }
//!   DELETE /api/weights           — delete a weight { kind, target }
//!   GET    /api/weights/suggest   — recency suggestions ?days=30

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
pub(crate) struct WeightsListQuery {
    kind: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct SuggestQuery {
    days: Option<i64>,
}

#[derive(Serialize)]
struct WeightDto {
    target_kind: String,
    target: String,
    weight: f32,
    source: String,
    reason: Option<String>,
    updated_at: i64,
}

#[derive(Deserialize)]
pub(crate) struct SetWeightBody {
    target_kind: String,
    target: String,
    weight: f32,
    reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct DeleteWeightBody {
    target_kind: String,
    target: String,
}

pub(crate) async fn api_weights_list(
    State(state): State<AppState>,
    Query(q): Query<WeightsListQuery>,
) -> Response {
    let store = state.store.lock().await;
    match store.list_weights(q.kind.as_deref()) {
        Ok(weights) => Json(
            weights
                .into_iter()
                .map(|w| WeightDto {
                    target_kind: w.target_kind,
                    target: w.target,
                    weight: w.weight,
                    source: w.source,
                    reason: w.reason,
                    updated_at: w.updated_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_weights_set(
    State(state): State<AppState>,
    Json(body): Json<SetWeightBody>,
) -> Response {
    if body.weight < 0.0 {
        return err_json(StatusCode::BAD_REQUEST, "weight must be ≥ 0.0");
    }
    let valid_kinds = ["file", "dir", "category"];
    if !valid_kinds.contains(&body.target_kind.as_str()) {
        return err_json(
            StatusCode::BAD_REQUEST,
            "target_kind must be 'file', 'dir', or 'category'",
        );
    }
    let mut store = state.store.lock().await;
    match store.set_weight(
        &body.target_kind,
        &body.target,
        body.weight,
        "user",
        body.reason.as_deref(),
    ) {
        Ok(()) => {
            Json(serde_json::json!({ "set": true, "target": body.target, "weight": body.weight }))
                .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_weights_delete(
    State(state): State<AppState>,
    Json(body): Json<DeleteWeightBody>,
) -> Response {
    let mut store = state.store.lock().await;
    match store.delete_weight(&body.target_kind, &body.target) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_weights_suggest(
    State(state): State<AppState>,
    Query(q): Query<SuggestQuery>,
) -> Response {
    let days = q.days.unwrap_or(30).max(1);
    let store = state.store.lock().await;
    match store.suggest_weights_by_recency(days) {
        Ok(suggestions) => {
            let items: Vec<serde_json::Value> = suggestions
                .into_iter()
                .map(|(path, w)| serde_json::json!({ "path": path, "weight": w }))
                .collect();
            Json(serde_json::json!({ "days": days, "suggestions": items })).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
