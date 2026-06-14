//! `GET /api/inspect?path=` — the web equivalent of `indexa inspect`: a plain "what's indexed
//! here" view (entry facts, chunk count, summary presence, classification, weight, code-graph
//! edges) so the index is legible in the UI, not a black box. Read-only; reuses the same store
//! reads the CLI `cmd_inspect` uses.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::dto::{err_json, InspectResponse};
use crate::AppState;

#[derive(Deserialize)]
pub(crate) struct InspectQuery {
    path: String,
}

pub(crate) async fn api_inspect(
    State(state): State<AppState>,
    Query(q): Query<InspectQuery>,
) -> Response {
    if q.path.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "missing 'path' query parameter");
    }
    let store = state.store.lock().await;

    let entry = store.entry_by_path(&q.path).ok().flatten();
    let summary = store.summary_by_path(&q.path).ok().flatten();
    let chunks = store.chunks_for_path(&q.path, 0).unwrap_or_default();

    if entry.is_none() && summary.is_none() && chunks.is_empty() {
        return err_json(StatusCode::NOT_FOUND, "nothing indexed at this path");
    }

    let classification = store.classification_for(&q.path).ok().flatten();
    let weight = store.weight_for(&q.path).unwrap_or(1.0);
    let edges = store.edges_from(&q.path).unwrap_or_default();
    let count_kind = |k: &str| edges.iter().filter(|e| e.kind == k).count();

    let language = chunks.iter().find_map(|c| c.language.clone());
    let chunk_headings: Vec<String> = chunks
        .iter()
        .take(8)
        .map(|c| {
            if c.heading.trim().is_empty() {
                "(no heading)".to_owned()
            } else {
                c.heading.clone()
            }
        })
        .collect();

    Json(InspectResponse {
        path: q.path.clone(),
        kind: entry.as_ref().map(|e| e.kind.clone()),
        size: entry.as_ref().map(|e| e.size),
        modified_s: entry.as_ref().and_then(|e| e.modified_s),
        chunk_count: chunks.len(),
        chunk_headings,
        language,
        has_summary: summary.is_some(),
        abstract_: summary.as_ref().and_then(|s| s.summary_l0.clone()),
        summary_model: summary.as_ref().map(|s| s.model.clone()),
        category: classification.as_ref().map(|c| c.category.clone()),
        confidence: classification.as_ref().map(|c| c.confidence),
        weight,
        imports: count_kind("imports"),
        defines: count_kind("defines"),
        calls: count_kind("calls"),
    })
    .into_response()
}
