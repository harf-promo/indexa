use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{
    err_json, file_name_of, MapRow, RootResponse, SearchQuery, StatsResponse, TreeNodeResponse,
};
use crate::AppState;

pub(crate) async fn api_stats(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match (store.entry_count(), store.chunk_count()) {
        (Ok(entries), Ok(chunks)) => Json(StatsResponse { entries, chunks }).into_response(),
        (Err(e), _) | (_, Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_map(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.region_summary() {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|r| MapRow {
                    category: r.category,
                    entry_count: r.entry_count,
                    total_size: r.total_size,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_roots(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.root_paths() {
        Ok(paths) => Json(
            paths
                .into_iter()
                .map(|p| RootResponse {
                    name: file_name_of(&p),
                    path: p,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Response {
    let q = params.q.as_deref().unwrap_or("").trim().to_owned();
    if q.is_empty() {
        return Json(Vec::<TreeNodeResponse>::new()).into_response();
    }
    let limit = params.limit.unwrap_or(50).min(200);
    let store = state.store.lock().await;
    match store.search_paths(&q, limit) {
        Ok(nodes) => Json(
            nodes
                .into_iter()
                .map(TreeNodeResponse::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
