//! `GET /api/export` — download the summary tree as XML, Markdown, or JSON.
//!
//! Reuses the same rendering primitives as `indexa export` CLI (all in
//! `indexa_query`). Export is synchronous (reads pre-computed summaries, no
//! LLM calls) and fast enough for a direct HTTP response — no job/queue needed.

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dto::err_json;
use crate::AppState;

#[derive(Deserialize)]
pub(crate) struct ExportQuery {
    /// Absolute path to export (optional — defaults to all indexed roots).
    path: Option<String>,
    /// Output format: xml (default), md, json
    format: Option<String>,
    /// Maximum tree depth (0 = root summary only; omit for full depth).
    depth: Option<usize>,
    /// Emit a code-skeleton view (symbol signatures, bodies elided) instead of prose summaries.
    signatures: Option<bool>,
    /// Relational slice: only files modified within this window (e.g. `7d`, `12h`, `90m`).
    changed_since: Option<String>,
    /// Relational slice: only files in this classification category (e.g. `code`, `document`).
    category: Option<String>,
}

pub(crate) async fn api_export(
    State(state): State<AppState>,
    Query(params): Query<ExportQuery>,
) -> Response {
    let store = state.store.lock().await;

    // Resolve roots: explicit path or all indexed roots.
    let roots: Vec<String> = match &params.path {
        Some(p) if !p.trim().is_empty() => vec![p.clone()],
        _ => match store.tree_level("") {
            Ok(nodes) => nodes.into_iter().map(|n| n.path).collect(),
            Err(e) => {
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to list roots: {e:#}"),
                )
            }
        },
    };

    if roots.is_empty() {
        return err_json(
            StatusCode::NOT_FOUND,
            "No summaries found. Run `indexa summarize <path>` first.",
        );
    }

    let now_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now = now_secs.to_string();

    let fmt = params.format.as_deref().unwrap_or("xml");
    let signatures = params.signatures.unwrap_or(false);

    // Relational slice (v0.60): same `--changed-since` / `--category` filters as CLI `export`,
    // shared via `build_export_filter`. `None` ⇒ export everything; a bad duration → 400.
    let allow = match indexa_query::build_export_filter(
        &store,
        params.changed_since.as_deref(),
        params.category.as_deref(),
        now_secs,
    ) {
        Ok(a) => a,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };

    let mut out_buf = String::new();
    for root_path in &roots {
        if signatures {
            match store.code_chunks_under(root_path, 0) {
                Ok(mut chunks) => {
                    if let Some(a) = &allow {
                        chunks.retain(|c| a.contains(&c.entry_path));
                    }
                    if !chunks.is_empty() {
                        out_buf.push_str(&indexa_query::render_signatures(&chunks, fmt, true));
                        out_buf.push('\n');
                    }
                }
                Err(e) => {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Export failed for {root_path}: {e:#}"),
                    )
                }
            }
            continue;
        }
        match indexa_query::build_tree(&store, root_path, params.depth) {
            Ok(Some(tree)) => {
                // Apply the relational slice; skip a root that matched nothing.
                let tree = match &allow {
                    Some(a) => match indexa_query::prune_tree(tree, a) {
                        Some(t) => t,
                        None => continue,
                    },
                    None => tree,
                };
                let rendered = match fmt {
                    "md" | "markdown" => indexa_query::render_markdown(&tree),
                    "json" => indexa_query::render_json(&tree),
                    _ => indexa_query::render_xml(&tree, &now),
                };
                out_buf.push_str(&rendered);
                out_buf.push('\n');
            }
            Ok(None) => {
                // Path has no summary yet — skip silently (consistent with CLI behaviour)
            }
            Err(e) => {
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Export failed for {root_path}: {e:#}"),
                )
            }
        }
    }

    if out_buf.is_empty() {
        let msg = if allow.is_some() {
            "Nothing matched the export slice (changed_since / category). \
             Widen the window/category or drop the filter."
        } else {
            "No summaries found for the requested path(s). \
             Run `indexa summarize <path>` first."
        };
        return err_json(StatusCode::NOT_FOUND, msg);
    }

    // Scan exported content for secrets before it leaves the machine over HTTP.
    let (out_buf, _redacted) = indexa_query::redact::redact_secrets(&out_buf);

    let (content_type, ext) = match fmt {
        "md" | "markdown" => ("text/markdown; charset=utf-8", "md"),
        "json" => ("application/json; charset=utf-8", "json"),
        _ => ("application/xml; charset=utf-8", "xml"),
    };

    let filename = format!("indexa-context.{ext}");

    (
        [
            (header::CONTENT_TYPE, content_type.to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        out_buf,
    )
        .into_response()
}
