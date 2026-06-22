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
    /// Center the graph on this file path and return only its N-hop neighborhood
    /// (the "expand a node's neighbors" interaction). Filters the already-scoped
    /// graph server-side so a hub's real neighbors aren't lost to client-side
    /// truncation. Empty/unset = whole scope.
    focus: Option<String>,
    /// Hops from `focus` to include, clamped to `[1, 2]`. Ignored without `focus`.
    /// Default 1 (direct callers + callees only).
    #[serde(default)]
    depth: Option<usize>,
    /// Knowledge-graph layers to overlay on the call graph (comma-separated): `"semantic"`
    /// (meaning-similarity edges), `"category"` (files sharing a classification category),
    /// `"pack"` (files in the same Context Pack), and/or `"communities"` (Louvain clustering of the
    /// call graph — colours nodes by community, surfaces hubs + bridge edges). Omit ⇒ call graph
    /// only, byte-identical to before. Read-only, derived at request time.
    #[serde(default)]
    layers: Option<String>,
    /// Cosine threshold for `semantic` edges (default 0.78). Higher ⇒ fewer, tighter edges.
    #[serde(default)]
    sim_threshold: Option<f32>,
    /// Skip the O(n²) semantic pass when the displayed node count exceeds this (default 300).
    #[serde(default)]
    sim_max_nodes: Option<usize>,
}

/// Keep only `focus` plus the nodes within `depth` undirected hops of it, dropping
/// the rest and the edges that no longer connect two kept nodes. Pure in-memory
/// filtering of an already-fetched scoped graph — no DB access, no schema change.
/// `edge_tiers` is parallel to `edges` and is filtered in lockstep.
fn apply_focus(sg: &mut indexa_core::store::ScopedCodeGraph, focus: &str, depth: usize) {
    use std::collections::{HashMap, HashSet, VecDeque};

    // Build undirected adjacency, then BFS the kept set. `adj` borrows the edges
    // immutably; it's dropped before we mutate the graph below.
    let mut keep: HashSet<String> = HashSet::new();
    {
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in &sg.graph.edges {
            adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
            adj.entry(e.to.as_str()).or_default().push(e.from.as_str());
        }
        keep.insert(focus.to_owned());
        let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
        frontier.push_back((focus.to_owned(), 0));
        while let Some((cur, dist)) = frontier.pop_front() {
            if dist >= depth {
                continue;
            }
            if let Some(ns) = adj.get(cur.as_str()) {
                for &n in ns {
                    if keep.insert(n.to_owned()) {
                        frontier.push_back((n.to_owned(), dist + 1));
                    }
                }
            }
        }
    }

    sg.graph.nodes.retain(|n| keep.contains(&n.path));
    let edges = std::mem::take(&mut sg.graph.edges);
    let tiers = std::mem::take(&mut sg.edge_tiers);
    for (e, t) in edges.into_iter().zip(tiers) {
        if keep.contains(&e.from) && keep.contains(&e.to) {
            sg.graph.edges.push(e);
            sg.edge_tiers.push(t);
        }
    }
}

#[derive(Serialize)]
struct NodeDto {
    path: String,
    label: String,
    out_degree: usize,
    in_degree: usize,
    /// Weighted PageRank centrality over the displayed subgraph (sums to ~1.0).
    pagerank: f64,
    /// Community id (Louvain over the displayed call graph) when the `communities` layer is on;
    /// omitted otherwise ⇒ byte-identical default response.
    #[serde(skip_serializing_if = "Option::is_none")]
    community: Option<usize>,
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
    /// `true` when this (structural) edge crosses two communities — a "surprising connection".
    /// Set only when the `communities` layer is on; omitted (false) otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    bridge: bool,
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// Compute one knowledge-graph overlay layer (semantic / category / pack) when `layers` requests
/// `name`. The `fetch` closure opens its OWN fresh `Store` and returns `(from, to, weight)` triples;
/// it runs inside `spawn_blocking` (never the shared mutex) and the whole layer is **fail-open** —
/// any error (or the layer not being requested) yields no edges, leaving the call graph untouched.
/// Returns `(count, edges)` tagged `tier = name`. Single source of truth for the per-layer pattern
/// so a future layer can't silently skip the fresh-conn / fail-open / cost-guard discipline.
async fn overlay_layer<F>(
    layers: Option<&str>,
    name: &'static str,
    fetch: F,
) -> (usize, Vec<EdgeDto>)
where
    F: FnOnce() -> anyhow::Result<Vec<(String, String, usize)>> + Send + 'static,
{
    let want = layers
        .map(|l| l.split(',').any(|x| x.trim() == name))
        .unwrap_or(false);
    if !want {
        return (0, Vec::new());
    }
    let res = tokio::task::spawn_blocking(fetch)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{name} task panicked: {e}")));
    match res {
        Ok(triples) => {
            let dtos: Vec<EdgeDto> = triples
                .into_iter()
                .map(|(from, to, weight)| EdgeDto {
                    from,
                    to,
                    weight,
                    tier: name.to_owned(),
                    bridge: false,
                })
                .collect();
            (dtos.len(), dtos)
        }
        Err(_) => (0, Vec::new()),
    }
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
        Ok(mut sg) => {
            // Optional focus: keep only the focus node's N-hop neighborhood.
            if let Some(focus) = q.focus.filter(|s| !s.is_empty()) {
                let depth = q.depth.unwrap_or(1).clamp(1, 2);
                apply_focus(&mut sg, &focus, depth);
            }
            let g = sg.graph;
            let mut nodes: Vec<NodeDto> = g
                .nodes
                .into_iter()
                .map(|n| NodeDto {
                    label: basename(&n.path),
                    path: n.path,
                    out_degree: n.out_degree,
                    in_degree: n.in_degree,
                    pagerank: n.pagerank,
                    community: None,
                })
                .collect();
            // edge_tiers is parallel to edges (same order, same length).
            let mut edges: Vec<EdgeDto> = g
                .edges
                .into_iter()
                .zip(sg.edge_tiers.iter())
                .map(|(e, tier)| EdgeDto {
                    from: e.from,
                    to: e.to,
                    weight: e.weight,
                    bridge: false,
                    tier: tier.as_str().to_owned(),
                })
                .collect();
            let bare_edges = sg.edge_tiers.iter().filter(|t| t.is_bare()).count();

            // Optional knowledge-graph overlays (semantic / category / pack): each adds file→file
            // edges of its tier, computed on a fresh connection + fail-open via `overlay_layer`.
            // `node_paths` is built once (only when an edge layer is requested) and shared.
            let layers = q.layers.as_deref();
            let edge_layer_wanted = layers
                .map(|l| {
                    l.split(',')
                        .any(|x| matches!(x.trim(), "semantic" | "category" | "pack"))
                })
                .unwrap_or(false);
            let node_paths: Vec<String> = if edge_layer_wanted {
                nodes.iter().map(|n| n.path.clone()).collect()
            } else {
                Vec::new()
            };
            let max_nodes = q.sim_max_nodes.unwrap_or(300);

            // Semantic: meaning-similarity edges (cosine over chunk centroids); weight from similarity.
            let (semantic_edges, sem) = {
                let db = state.db_path.clone();
                let scope_c = scope.clone();
                let np = node_paths.clone();
                let threshold = q.sim_threshold.unwrap_or(0.78);
                overlay_layer(layers, "semantic", move || {
                    let store = indexa_core::store::Store::open(&db)?;
                    Ok(store
                        .semantic_file_edges(&scope_c, &np, threshold, max_nodes)?
                        .into_iter()
                        .map(|(f, t, sim)| (f, t, (sim * 10.0).round().max(1.0) as usize))
                        .collect())
                })
                .await
            };
            edges.extend(sem);

            // Category: files sharing a confirmed classification (deterministic star per category).
            let (category_edges, cat) = {
                let db = state.db_path.clone();
                let np = node_paths.clone();
                overlay_layer(layers, "category", move || {
                    let store = indexa_core::store::Store::open(&db)?;
                    Ok(store
                        .category_file_edges(&np, max_nodes)?
                        .into_iter()
                        .map(|(f, t)| (f, t, 1))
                        .collect())
                })
                .await
            };
            edges.extend(cat);

            // Pack: files in the same Context Pack (exact user curation; star per pack).
            let (pack_edges, pck) = {
                let db = state.db_path.clone();
                let np = node_paths.clone();
                overlay_layer(layers, "pack", move || {
                    let store = indexa_core::store::Store::open(&db)?;
                    Ok(store
                        .pack_file_edges(&np, max_nodes)?
                        .into_iter()
                        .map(|(f, t)| (f, t, 1))
                        .collect())
                })
                .await
            };
            edges.extend(pck);

            // Optional "communities" overlay: Louvain over the displayed STRUCTURAL call graph
            // (excluding the semantic/category/pack overlay edges, so community membership is
            // stable regardless of which overlays are on). Surfaces a hub per community and marks
            // cross-community "bridge" edges. Computed inline (≤2000 nodes ⇒ sub-ms); fail-open.
            let want_communities = q
                .layers
                .as_deref()
                .map(|l| l.split(',').any(|x| x.trim() == "communities"))
                .unwrap_or(false);
            let mut communities_json: Vec<serde_json::Value> = Vec::new();
            let mut bridge_edges = 0usize;
            if want_communities {
                // Owned keys so the map doesn't borrow `nodes` (which we mutate below).
                let idx: std::collections::HashMap<String, usize> = nodes
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.path.clone(), i))
                    .collect();
                let is_overlay = |t: &str| matches!(t, "semantic" | "category" | "pack");
                let call_pairs: Vec<(usize, usize)> = edges
                    .iter()
                    .filter(|e| !is_overlay(&e.tier))
                    .filter_map(|e| Some((*idx.get(e.from.as_str())?, *idx.get(e.to.as_str())?)))
                    .collect();
                let labels = indexa_core::store::detect_communities(nodes.len(), &call_pairs);
                if !labels.is_empty() {
                    for (i, nd) in nodes.iter_mut().enumerate() {
                        nd.community = Some(labels[i]);
                    }
                    // Mark structural bridge edges (cross-community "surprising connections").
                    for e in edges.iter_mut() {
                        if is_overlay(&e.tier) {
                            continue;
                        }
                        if let (Some(&a), Some(&b)) =
                            (idx.get(e.from.as_str()), idx.get(e.to.as_str()))
                        {
                            if labels[a] != labels[b] {
                                e.bridge = true;
                                bridge_edges += 1;
                            }
                        }
                    }
                    // Per-community hub (max PageRank, smallest-path tie-break) + size.
                    let k = labels.iter().copied().max().map(|m| m + 1).unwrap_or(0);
                    let mut hub: Vec<Option<usize>> = vec![None; k];
                    let mut size = vec![0usize; k];
                    for (i, &c) in labels.iter().enumerate() {
                        size[c] += 1;
                        hub[c] = Some(match hub[c] {
                            None => i,
                            Some(h) => {
                                let better = nodes[i].pagerank > nodes[h].pagerank
                                    || (nodes[i].pagerank == nodes[h].pagerank
                                        && nodes[i].path < nodes[h].path);
                                if better {
                                    i
                                } else {
                                    h
                                }
                            }
                        });
                    }
                    let mut comms: Vec<(usize, String, usize)> = (0..k)
                        .filter_map(|c| hub[c].map(|h| (c, nodes[h].path.clone(), size[c])))
                        .collect();
                    comms.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
                    communities_json = comms
                        .into_iter()
                        .map(|(id, hub_path, sz)| {
                            serde_json::json!({ "id": id, "hub_path": hub_path, "size": sz })
                        })
                        .collect();
                }
            }

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
                // Knowledge-graph overlays (additive; 0 / absent layer ⇒ call graph only).
                "semantic_edges": semantic_edges,
                "category_edges": category_edges,
                "pack_edges": pack_edges,
                // Community detection (empty/0 unless the `communities` layer is on).
                "communities": communities_json,
                "bridge_edges": bridge_edges,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::store::{
        CodeGraph, CodeGraphEdge, CodeGraphNode, ResolutionTier, ScopedCodeGraph,
    };

    fn node(p: &str) -> CodeGraphNode {
        CodeGraphNode {
            path: p.into(),
            out_degree: 0,
            in_degree: 0,
            pagerank: 0.1,
        }
    }
    fn edge(f: &str, t: &str) -> CodeGraphEdge {
        CodeGraphEdge {
            from: f.into(),
            to: t.into(),
            weight: 1,
        }
    }
    // a→b→c→d  and  a→e  (undirected adjacency for focus BFS)
    fn sample() -> ScopedCodeGraph {
        ScopedCodeGraph {
            graph: CodeGraph {
                nodes: vec![node("a"), node("b"), node("c"), node("d"), node("e")],
                edges: vec![
                    edge("a", "b"),
                    edge("b", "c"),
                    edge("c", "d"),
                    edge("a", "e"),
                ],
                truncated: false,
            },
            edge_tiers: vec![
                ResolutionTier::Import,
                ResolutionTier::Bare,
                ResolutionTier::SameDir,
                ResolutionTier::Import,
            ],
        }
    }
    fn paths(sg: &ScopedCodeGraph) -> Vec<String> {
        let mut v: Vec<String> = sg.graph.nodes.iter().map(|n| n.path.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn focus_depth_1_keeps_direct_neighbors_only() {
        let mut sg = sample();
        apply_focus(&mut sg, "a", 1);
        assert_eq!(paths(&sg), vec!["a", "b", "e"]);
        // Only edges with both endpoints kept survive — a→b and a→e.
        assert_eq!(sg.graph.edges.len(), 2);
        assert!(sg.graph.edges.iter().all(|e| e.from == "a"));
        // edge_tiers stays parallel to edges (same length, lockstep filter).
        assert_eq!(sg.edge_tiers.len(), sg.graph.edges.len());
    }

    #[test]
    fn focus_depth_2_widens_one_more_hop() {
        let mut sg = sample();
        apply_focus(&mut sg, "a", 2);
        assert_eq!(paths(&sg), vec!["a", "b", "c", "e"]);
        // a→b, b→c, a→e kept; c→d dropped (d is 3 hops away).
        assert_eq!(sg.graph.edges.len(), 3);
        assert_eq!(sg.edge_tiers.len(), 3);
    }

    #[test]
    fn focus_isolated_node_returns_just_it() {
        let mut sg = ScopedCodeGraph {
            graph: CodeGraph {
                nodes: vec![node("solo"), node("other")],
                edges: vec![],
                truncated: false,
            },
            edge_tiers: vec![],
        };
        apply_focus(&mut sg, "solo", 2);
        assert_eq!(paths(&sg), vec!["solo"]);
        assert!(sg.graph.edges.is_empty());
    }

    #[test]
    fn focus_unknown_path_drops_everything_without_panic() {
        let mut sg = sample();
        apply_focus(&mut sg, "not-a-node", 2);
        assert!(sg.graph.nodes.is_empty());
        assert!(sg.graph.edges.is_empty());
        assert!(sg.edge_tiers.is_empty());
    }
}
