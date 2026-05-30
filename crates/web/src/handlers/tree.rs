use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{err_json, PathQuery, TreeNodeResponse};
use crate::AppState;

pub(crate) async fn api_tree(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = params.path.as_deref().unwrap_or("");
    let store = state.store.lock().await;
    match store.tree_level(path) {
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
