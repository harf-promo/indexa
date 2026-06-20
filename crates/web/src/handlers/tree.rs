use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use indexa_core::store::Store;

use crate::dto::{err_json, PathQuery, TreeNodeResponse};
use crate::AppState;

pub(crate) async fn api_tree(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = params.path.as_deref().unwrap_or("");
    // Open a fresh, short-lived read connection instead of locking the shared
    // store for the whole query — a slow tree expansion no longer serializes every
    // other web request that needs the store (mirrors how the MCP tools open a
    // per-call connection). `tree_level` is read-only.
    let store = match Store::open(&state.db_path) {
        Ok(s) => s,
        Err(e) => {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
    };
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
