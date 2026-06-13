//! Signature graph REST API (v0.18).
//!
//! Route: GET /api/graph?scope=<path>&limit=<n>
//! Returns the file-to-file call graph (file A → file B when A calls a function B defines)
//! for files under `scope`. The JOIN can be heavy on a large graph, so it runs on a fresh
//! connection inside spawn_blocking — never holding the shared store mutex.

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
pub(crate) struct GraphQuery {
    scope: Option<String>,
    limit: Option<usize>,
    /// Strict resolution: only link calls to uniquely-defined symbols (default false =
    /// the broader bare-name match, which is what PageRank / Map node sizing expect).
    #[serde(default)]
    strict: bool,
}

#[derive(Serialize)]
struct NodeDto {
    path: String,
    label: String,
    out_degree: usize,
    in_degree: usize,
    /// Weighted PageRank centrality over the displayed subgraph (sums to ~1.0).
    pagerank: f64,
}

#[derive(Serialize)]
struct EdgeDto {
    from: String,
    to: String,
    weight: usize,
    /// Resolution tier: `same_file` / `same_dir` / `import` (scoped) or `bare`
    /// (approximate name-only match). Lets the Map render scoped vs bare edges
    /// distinctly and apply the bare-name caveat only where it belongs.
    tier: String,
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

pub(crate) async fn api_graph(
    State(state): State<AppState>,
    Query(q): Query<GraphQuery>,
) -> Response {
    // Default scope: the largest indexed root (first /api/roots entry) when unset.
    let scope = match q.scope.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            let store = state.store.lock().await;
            match store.root_paths() {
                Ok(roots) if !roots.is_empty() => roots[0].clone(),
                Ok(_) => {
                    return Json(serde_json::json!({
                        "scope": "",
                        "nodes": [],
                        "edges": [],
                        "truncated": false,
                    }))
                    .into_response()
                }
                Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
            }
        }
    };
    let limit = q.limit.unwrap_or(400).clamp(1, 2000);
    let strict = q.strict;

    let db_path = state.db_path.clone();
    let scope_for_task = scope.clone();
    let scoped = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let store = indexa_core::store::Store::open(&db_path)?;
        store.code_graph_scoped(&scope_for_task, limit, strict)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("graph task panicked: {e}")));

    match scoped {
        Ok(sg) => {
            let g = sg.graph;
            let nodes: Vec<NodeDto> = g
                .nodes
                .into_iter()
                .map(|n| NodeDto {
                    label: basename(&n.path),
                    path: n.path,
                    out_degree: n.out_degree,
                    in_degree: n.in_degree,
                    pagerank: n.pagerank,
                })
                .collect();
            // edge_tiers is parallel to edges (same order, same length).
            let edges: Vec<EdgeDto> = g
                .edges
                .into_iter()
                .zip(sg.edge_tiers.iter())
                .map(|(e, tier)| EdgeDto {
                    from: e.from,
                    to: e.to,
                    weight: e.weight,
                    tier: tier.as_str().to_owned(),
                })
                .collect();
            let bare_edges = sg.edge_tiers.iter().filter(|t| t.is_bare()).count();
            Json(serde_json::json!({
                "scope": scope,
                "nodes": nodes,
                "edges": edges,
                "truncated": g.truncated,
                // The bare-name caveat applies only to the bare remainder; the UI
                // shows it conditionally on this count.
                "bare_edges": bare_edges,
                // In strict mode bare edges are *dropped*, not resolved — so a zero
                // bare count here means "filtered out", which the UI must not report
                // as "all scope-resolved". Echo the flag so it can word it honestly.
                "strict": strict,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
