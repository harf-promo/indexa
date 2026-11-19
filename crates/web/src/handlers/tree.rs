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
    let db_path = state.db_path.clone();
    let path = path.to_owned();
    match tokio::task::spawn_blocking(move || {
        let store = Store::open(&db_path)?;
        store.tree_level(&path)
    })
    .await
    {
        Ok(Ok(nodes)) => Json(
            nodes
                .into_iter()
                .map(TreeNodeResponse::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tree worker failed: {e}"),
        ),
    }
}
