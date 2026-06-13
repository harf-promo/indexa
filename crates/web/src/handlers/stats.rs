use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{
    err_json, file_name_of, CoverageStats, ImpactResponse, RootResponse, SearchQuery,
    StatsResponse, ToolUsageDto, TreeNodeResponse, TreemapNodeDto, UsageWeekDto,
};
use crate::AppState;
use indexa_core::store::{CoverageEntry, USAGE_WEEK_SECS};

pub(crate) async fn api_stats(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    // Telemetry read is best-effort: zeros (the UI hides the line) over a 500.
    let usage = store.usage_summary(USAGE_WEEK_SECS).unwrap_or_default();
    match (store.entry_count(), store.chunk_count()) {
        (Ok(entries), Ok(chunks)) => Json(StatsResponse {
            entries,
            chunks,
            // Best-effort: 0 over a read error just hides the "context not built" hint.
            summaries: store.summary_count().unwrap_or(0),
            usage_week: UsageWeekDto {
                served: usage.bytes_served,
                counterfactual: usage.bytes_counterfactual,
            },
        })
        .into_response(),
        (Err(e), _) | (_, Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `GET /api/impact` — the token-savings "Impact" dashboard: weekly totals plus a
/// per-tool breakdown. Fetched lazily when the Settings → Impact section opens, so
/// the per-tool aggregate stays off the frequently-polled `/api/stats` path. The
/// numbers are estimates by definition (see `store::usage`); the UI carries the
/// ≈4 bytes/token caveat from `savings_line`.
pub(crate) async fn api_impact(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    // Best-effort reads: an empty/zero dashboard over a 500 — telemetry must never
    // be the reason a settings panel fails to render.
    let week = store.usage_summary(USAGE_WEEK_SECS).unwrap_or_default();
    let by_tool = store.usage_by_tool(USAGE_WEEK_SECS).unwrap_or_default();
    Json(ImpactResponse {
        calls: week.calls,
        served: week.bytes_served,
        counterfactual: week.bytes_counterfactual,
        savings_line: week.savings_line(),
        by_tool: by_tool
            .into_iter()
            .map(|(tool, u)| ToolUsageDto {
                tool,
                calls: u.calls,
                served: u.bytes_served,
                counterfactual: u.bytes_counterfactual,
            })
            .collect(),
    })
    .into_response()
}

/// Coverage breakdown for the Map → Table view.
pub(crate) async fn api_map(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.coverage_stats() {
        Ok((total_dirs, built, partial, failed, none, total_chunks, total_files)) => {
            Json(CoverageStats {
                total_dirs,
                built,
                partial,
                failed,
                none,
                total_chunks,
                total_files,
            })
            .into_response()
        }
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
        Ok(nodes) => {
            // Deliberately NO usage telemetry here: this is the sidebar
            // path-typeahead (a human navigating, debounced per keystroke),
            // not retrieval serving content to an AI client. Counting the
            // matched files' full sizes as "avoided reading" would inflate
            // the tokens-saved metric with every keystroke pause — a skeptic
            // could falsify the headline number by typing in the sidebar.
            let out: Vec<TreeNodeResponse> =
                nodes.into_iter().map(TreeNodeResponse::from).collect();
            Json(out).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Build and return the context-coverage treemap (sized by chunk count, colored by coverage).
pub(crate) async fn api_map_treemap(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.all_coverage_entries() {
        Ok(entries) => Json(build_coverage_treemap(entries, 4, 30)).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

// ── Coverage treemap builder ──────────────────────────────────────────────────

struct CoverageNodeData {
    parent: String,
    is_dir: bool,
    /// Subtree chunk count (propagated bottom-up).
    subtree_chunks: u64,
    /// Number of direct file children (for file_count label).
    file_count: u64,
    /// This node's own summary queue state.
    own_state: Option<String>,
    children: Vec<String>,
}

/// Map a `summary_queue.state` value (or `None` = no row) to a human-readable coverage label.
fn coverage_from_state(state: Option<&str>) -> &'static str {
    match state {
        Some("done") => "full",
        Some("pending") | Some("in_flight") => "partial",
        Some("failed") => "failed",
        _ => "none",
    }
}

/// Build a coverage treemap from `all_coverage_entries` data.
/// Cells are sized by subtree chunk count; coverage state drives JS coloring.
fn build_coverage_treemap(
    entries: Vec<CoverageEntry>,
    max_depth: usize,
    max_children: usize,
) -> Vec<TreemapNodeDto> {
    use std::collections::{HashMap, HashSet};

    let mut nodes: HashMap<String, CoverageNodeData> = HashMap::with_capacity(entries.len() + 64);

    // Phase 1: insert all explicit entries
    for (path, parent, is_dir, chunk_count, state) in &entries {
        nodes.insert(
            path.clone(),
            CoverageNodeData {
                parent: parent.clone(),
                is_dir: *is_dir,
                // Files contribute their own chunk count; dirs start at 0 (will be propagated).
                subtree_chunks: if *is_dir { 0 } else { *chunk_count },
                file_count: if *is_dir { 0 } else { 1 },
                own_state: state.clone(),
                children: Vec::new(),
            },
        );
    }

    // Phase 2: synthetic root nodes for parent paths absent from entries table
    let existing_paths: HashSet<&str> = nodes.keys().map(|s| s.as_str()).collect();
    let virtual_roots: HashSet<String> = entries
        .iter()
        .filter(|(_, parent, _, _, _)| {
            !parent.is_empty() && !existing_paths.contains(parent.as_str())
        })
        .map(|(_, parent, _, _, _)| parent.clone())
        .collect();
    for root in &virtual_roots {
        nodes.insert(
            root.clone(),
            CoverageNodeData {
                parent: String::new(),
                is_dir: true,
                subtree_chunks: 0,
                file_count: 0,
                own_state: None,
                children: Vec::new(),
            },
        );
    }

    // Phase 3: register children with their parents
    let child_registrations: Vec<(String, String)> = entries
        .iter()
        .filter(|(_, parent, _, _, _)| !parent.is_empty())
        .map(|(path, parent, _, _, _)| (path.clone(), parent.clone()))
        .collect();
    for (path, parent) in &child_registrations {
        if let Some(p) = nodes.get_mut(parent) {
            p.children.push(path.clone());
        }
    }

    // Phase 4: propagate chunk counts bottom-up (longest path first)
    let mut all_sorted: Vec<String> = nodes.keys().cloned().collect();
    all_sorted.sort_unstable_by_key(|b| std::cmp::Reverse(b.len()));
    for path in &all_sorted {
        let (chunks, count, parent) = {
            let n = &nodes[path];
            (n.subtree_chunks, n.file_count, n.parent.clone())
        };
        if !parent.is_empty() {
            if let Some(p) = nodes.get_mut(&parent) {
                p.subtree_chunks += chunks;
                p.file_count += count;
            }
        }
    }

    // Phase 5: find tree roots and emit the nested DTO
    let mut roots: Vec<&str> = nodes
        .iter()
        .filter(|(_, n)| n.is_dir && (n.parent.is_empty() || !nodes.contains_key(&n.parent)))
        .map(|(p, _)| p.as_str())
        .collect();
    roots.sort_unstable_by(|a, b| nodes[*b].subtree_chunks.cmp(&nodes[*a].subtree_chunks));

    roots
        .iter()
        .map(|r| build_coverage_node(r, &nodes, 0, max_depth, max_children))
        .collect()
}

fn build_coverage_node(
    path: &str,
    nodes: &std::collections::HashMap<String, CoverageNodeData>,
    depth: usize,
    max_depth: usize,
    max_children: usize,
) -> TreemapNodeDto {
    let node = &nodes[path];
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    let children = if depth < max_depth {
        let mut dirs: Vec<(&str, u64)> = node
            .children
            .iter()
            .filter(|c| nodes.get(c.as_str()).is_some_and(|n| n.is_dir))
            .map(|c| (c.as_str(), nodes[c.as_str()].subtree_chunks))
            .collect();
        dirs.sort_unstable_by_key(|b| std::cmp::Reverse(b.1));
        dirs.truncate(max_children);
        dirs.into_iter()
            .map(|(c, _)| build_coverage_node(c, nodes, depth + 1, max_depth, max_children))
            .collect()
    } else {
        Vec::new()
    };

    TreemapNodeDto {
        name,
        path: path.to_string(),
        size: node.subtree_chunks,
        file_count: node.file_count,
        coverage: coverage_from_state(node.own_state.as_deref()).to_owned(),
        children,
    }
}
