//! Context Packs REST API.
//!
//! Routes:
//!   GET    /api/packs                  — list all packs
//!   POST   /api/packs                  — create a pack  { name, description? }
//!   DELETE /api/packs/:name            — delete a pack by name
//!   GET    /api/packs/:name/paths      — list paths in a pack
//!   POST   /api/packs/:name/paths      — add paths     { paths: [...] }
//!   DELETE /api/packs/:name/paths      — remove paths  { paths: [...] }
//!   GET    /api/packs/:name/export     — export as XML/MD/JSON  ?format=&depth=
//!   POST   /api/packs/suggest          — suggest paths for a query { query, limit? }

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::dto::err_json;
use crate::AppState;

// ── DTOs ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PackDto {
    name: String,
    description: Option<String>,
    path_count: usize,
}

#[derive(Deserialize)]
pub(crate) struct CreatePackBody {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PathsBody {
    paths: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct ExportQuery {
    format: Option<String>,
    depth: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct SuggestBody {
    query: String,
    limit: Option<usize>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(crate) async fn api_packs_list(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.list_packs() {
        Ok(packs) => Json(
            packs
                .into_iter()
                .map(|p| PackDto {
                    name: p.name,
                    description: p.description,
                    path_count: p.path_count,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_create(
    State(state): State<AppState>,
    Json(body): Json<CreatePackBody>,
) -> Response {
    let mut store = state.store.lock().await;
    match store.create_pack(&body.name, body.description.as_deref()) {
        Ok(id) => Json(serde_json::json!({ "created": true, "id": id, "name": body.name }))
            .into_response(),
        Err(e) => {
            let msg = format!("{e:#}");
            // Surface unique-constraint violations as 409 so the UI can give a
            // clear "name already exists" message without parsing error text.
            if msg.contains("UNIQUE") {
                err_json(
                    StatusCode::CONFLICT,
                    format!("a pack named \"{}\" already exists", body.name),
                )
            } else {
                err_json(StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        }
    }
}

pub(crate) async fn api_packs_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.delete_pack(&pack.id) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_paths_get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.pack_paths(&pack.id) {
        Ok(paths) => Json(serde_json::json!({ "name": name, "paths": paths })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_paths_add(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PathsBody>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.add_pack_paths(&pack.id, &body.paths) {
        Ok(()) => Json(serde_json::json!({ "added": body.paths.len() })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_paths_remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PathsBody>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.remove_pack_paths(&pack.id, &body.paths) {
        Ok(()) => Json(serde_json::json!({ "removed": body.paths.len() })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_export(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    use indexa_query::{build_tree, render_json, render_markdown, render_xml};
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned());

    let format = q.format.as_deref().unwrap_or("xml");
    let depth = q.depth;

    let store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let paths = match store.pack_paths(&pack.id) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    if paths.is_empty() {
        return err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("pack \"{name}\" is empty — add paths first"),
        );
    }

    let is_xml = format != "md" && format != "markdown" && format != "json";
    let mut buf = String::new();
    if is_xml {
        buf.push_str("<context pack=\"");
        buf.push_str(&xml_escape(&name));
        buf.push_str("\" generated=\"");
        buf.push_str(&now);
        buf.push_str("\">\n");
    }

    let mut exported = 0usize;
    for root_path in &paths {
        let tree = match build_tree(&store, root_path, depth) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
        let rendered = match format {
            "md" | "markdown" => render_markdown(&tree),
            "json" => render_json(&tree),
            _ => render_xml(&tree, &now),
        };
        buf.push_str(&rendered);
        buf.push('\n');
        exported += 1;
    }
    if is_xml {
        buf.push_str("</context>\n");
    }

    if exported == 0 {
        return err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "no paths in pack \"{name}\" have summaries yet \
                 — run `indexa summarize <path>` first"
            ),
        );
    }

    let content_type = if format == "json" {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    ([(axum::http::header::CONTENT_TYPE, content_type)], buf).into_response()
}

pub(crate) async fn api_packs_suggest(
    State(state): State<AppState>,
    Json(body): Json<SuggestBody>,
) -> Response {
    let limit = body.limit.unwrap_or(20).min(100);
    let query = body.query.trim().to_owned();
    if query.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query must not be empty");
    }

    // Try semantic search via the embedder held in AppState.
    let embedding = state.embedder.embed(&query).await.ok();

    let store = state.store.lock().await;
    if let Some(emb) = embedding {
        match store.summary_cosine_search(&emb, limit, 0.15) {
            Ok(hits) if !hits.is_empty() => {
                let paths: Vec<&str> = hits.iter().map(|(p, _)| p.as_str()).collect();
                return Json(serde_json::json!({
                    "method": "semantic",
                    "paths": paths,
                }))
                .into_response();
            }
            Ok(_) => {} // fall through to keyword
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }

    // Keyword fallback.
    use indexa_core::config::HybridMode;
    match store.hybrid_search(&query, None, &HybridMode::Sparse, None, limit * 3, 0.0) {
        Ok(hits) => {
            let mut seen = std::collections::HashSet::new();
            let paths: Vec<&str> = hits
                .iter()
                .filter_map(|h| {
                    if seen.insert(h.entry_path.as_str()) {
                        Some(h.entry_path.as_str())
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect();
            Json(serde_json::json!({
                "method": "keyword",
                "paths": paths,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
