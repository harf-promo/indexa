use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{
    err_json, file_name_of, require_path, BreadcrumbResponse, PathQuery, SummaryChildResponse,
    SummaryResponse,
};
use crate::AppState;

pub(crate) async fn api_summary(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let store = state.store.lock().await;
    let rec = match store.summary_by_path(&path) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // 404 so HTTP clients can distinguish "not yet summarized" from a 500.
            // The JSON body preserves backward compat (`d.pending` check in JS).
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "no summary", "pending": true})),
            )
                .into_response();
        }
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };

    let children = store.children_summaries(&path).unwrap_or_default();
    let crumbs = store.ancestor_summaries(&path).unwrap_or_default();

    let child_responses: Vec<SummaryChildResponse> = children
        .into_iter()
        .map(|c| SummaryChildResponse {
            name: file_name_of(&c.path),
            path: c.path,
            kind: c.kind,
            abstract_: c.summary_l0.unwrap_or_default(),
            summary: c.summary,
            summary_state: Some("done".into()),
        })
        .collect();

    let crumb_responses: Vec<BreadcrumbResponse> = crumbs
        .into_iter()
        .map(|c| BreadcrumbResponse {
            name: file_name_of(&c.path),
            path: c.path,
            summary: c.summary,
        })
        .collect();

    Json(SummaryResponse {
        path: rec.path,
        kind: rec.kind,
        abstract_: rec.summary_l0.unwrap_or_default(),
        summary: rec.summary,
        model: rec.model,
        generated_at: rec.generated_at,
        children: child_responses,
        crumbs: crumb_responses,
    })
    .into_response()
}

pub(crate) async fn api_summarize_enqueue(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let depth = path.chars().filter(|&c| c == '/' || c == '\\').count() as i64;
    let kind = if std::path::Path::new(&path).is_dir() {
        "dir"
    } else {
        "file"
    };
    let mut store = state.store.lock().await;
    match store.enqueue_summary_items(&[(path.clone(), kind.into(), depth)]) {
        Ok(()) => Json(serde_json::json!({"queued":true,"path":path})).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
