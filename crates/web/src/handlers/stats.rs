use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{
    err_json, file_name_of, MapRow, RootResponse, SearchQuery, StatsResponse, TreeNodeResponse,
    TreemapNodeDto,
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

/// Build and return a depth-3 treemap tree from the entry index.
pub(crate) async fn api_map_treemap(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.all_entry_sizes() {
        Ok(entries) => Json(build_treemap(entries, 3, 25)).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

// ── Treemap tree builder ──────────────────────────────────────────────────────

struct NodeData {
    parent: String,
    is_dir: bool,
    subtree_size: u64,
    file_count: u64,
    children: Vec<String>,
}

fn build_treemap(
    entries: Vec<(String, String, bool, u64)>,
    max_depth: usize,
    max_children: usize,
) -> Vec<TreemapNodeDto> {
    use std::collections::{HashMap, HashSet};

    let mut nodes: HashMap<String, NodeData> = HashMap::with_capacity(entries.len() + 64);

    // Phase 1: insert all explicit entries
    for (path, parent, is_dir, size) in &entries {
        nodes.insert(
            path.clone(),
            NodeData {
                parent: parent.clone(),
                is_dir: *is_dir,
                subtree_size: if *is_dir { 0 } else { *size },
                file_count: if *is_dir { 0 } else { 1 },
                children: Vec::new(),
            },
        );
    }

    // Phase 2: create virtual root nodes for parent_paths absent from the entries table
    let existing_paths: HashSet<&str> = nodes.keys().map(|s| s.as_str()).collect();
    let virtual_roots: HashSet<String> = entries
        .iter()
        .filter(|(_, parent, _, _)| !parent.is_empty() && !existing_paths.contains(parent.as_str()))
        .map(|(_, parent, _, _)| parent.clone())
        .collect();
    for root in &virtual_roots {
        nodes.insert(
            root.clone(),
            NodeData {
                parent: String::new(),
                is_dir: true,
                subtree_size: 0,
                file_count: 0,
                children: Vec::new(),
            },
        );
    }

    // Phase 3: register children with their parents
    let child_registrations: Vec<(String, String)> = entries
        .iter()
        .filter(|(_, parent, _, _)| !parent.is_empty())
        .map(|(path, parent, _, _)| (path.clone(), parent.clone()))
        .collect();
    for (path, parent) in &child_registrations {
        if let Some(p) = nodes.get_mut(parent) {
            p.children.push(path.clone());
        }
    }

    // Phase 4: propagate sizes bottom-up (path length is a valid depth proxy)
    let mut all_sorted: Vec<String> = nodes.keys().cloned().collect();
    all_sorted.sort_unstable_by_key(|b| std::cmp::Reverse(b.len()));
    for path in &all_sorted {
        let (size, count, parent) = {
            let n = &nodes[path];
            (n.subtree_size, n.file_count, n.parent.clone())
        };
        if !parent.is_empty() {
            if let Some(p) = nodes.get_mut(&parent) {
                p.subtree_size += size;
                p.file_count += count;
            }
        }
    }

    // Phase 5: find roots and build the tree
    let mut roots: Vec<&str> = nodes
        .iter()
        .filter(|(_, n)| n.is_dir && (n.parent.is_empty() || !nodes.contains_key(&n.parent)))
        .map(|(p, _)| p.as_str())
        .collect();
    roots.sort_unstable_by(|a, b| nodes[*b].subtree_size.cmp(&nodes[*a].subtree_size));

    roots
        .iter()
        .map(|r| build_node(r, &nodes, 0, max_depth, max_children))
        .collect()
}

fn build_node(
    path: &str,
    nodes: &std::collections::HashMap<String, NodeData>,
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
            .map(|c| (c.as_str(), nodes[c.as_str()].subtree_size))
            .collect();
        dirs.sort_unstable_by_key(|b| std::cmp::Reverse(b.1));
        dirs.truncate(max_children);
        dirs.into_iter()
            .map(|(c, _)| build_node(c, nodes, depth + 1, max_depth, max_children))
            .collect()
    } else {
        Vec::new()
    };

    TreemapNodeDto {
        name,
        path: path.to_string(),
        size: node.subtree_size,
        file_count: node.file_count,
        children,
    }
}
