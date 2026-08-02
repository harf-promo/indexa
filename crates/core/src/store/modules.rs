//! Architecture-map clustering: group a scoped code graph into named "modules" via Louvain
//! (4.6, GitNexus communities/processes), boosted with directory priors so files that already
//! live together nudge the same way the call graph does — two files with no call edge between
//! them but sharing a parent directory are far more likely to be the same functional area than
//! two arbitrary unrelated files. Pure, dependency-free; persistence lives in
//! [`Store::replace_graph_modules`]/[`Store::graph_modules`] (this file has no DB access).

use super::{types::CodeGraph, Store};
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;

/// One cluster produced by [`cluster_with_directory_priors`], unlabeled — the caller (which has
/// LLM access; this crate doesn't) fills in `label` before persisting.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedModule {
    /// Dense `0..k` id, stable only within a single computation (not across recomputes).
    pub module_id: usize,
    /// Empty until the caller (which has LLM access; this crate doesn't) fills it in.
    pub label: String,
    /// Intra-cluster edge density: `2*e / (k*(k-1))` for `k>1` members, `1.0` for a singleton.
    pub cohesion: f64,
    /// Absolute paths of the cluster's member files, in graph order.
    pub members: Vec<String>,
}

/// Weight added between two files that share an immediate parent directory — a "prior" nudging
/// Louvain toward directory structure even where the call graph itself is sparse. Chosen small
/// relative to a typical shared-symbol call edge weight (usually ≥1) so a real call relationship
/// to a DIFFERENT directory can still outweigh mere directory adjacency.
const DIRECTORY_PRIOR_WEIGHT: usize = 1;

/// Cluster `graph`'s nodes into modules: builds an undirected weighted adjacency from the call
/// edges plus a same-directory prior, runs [`super::detect_communities`], then groups members and
/// computes per-cluster cohesion. Returns one [`ComputedModule`] per detected cluster, largest
/// first (ties broken by lowest member path) — callers that cap expensive per-cluster work (LLM
/// labeling) should slice this Vec, not re-sort it. Empty when `graph` has no nodes.
pub fn cluster_with_directory_priors(graph: &CodeGraph) -> Vec<ComputedModule> {
    let n = graph.nodes.len();
    if n == 0 {
        return Vec::new();
    }
    let idx: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.path.as_str(), i))
        .collect();

    let mut pairs: Vec<(usize, usize)> = Vec::with_capacity(graph.edges.len());
    for e in &graph.edges {
        if let (Some(&a), Some(&b)) = (idx.get(e.from.as_str()), idx.get(e.to.as_str())) {
            for _ in 0..e.weight.max(1) {
                pairs.push((a, b));
            }
        }
    }

    // Directory prior: one extra weighted edge per same-parent-directory pair. O(n^2) worst case,
    // guarded by the same MAX_NODES cap `detect_communities` itself enforces (a whole-repo call
    // graph is already capped to a bounded `max_edges` by the caller before this runs).
    let parents: Vec<Option<&Path>> = graph
        .nodes
        .iter()
        .map(|node| Path::new(&node.path).parent())
        .collect();
    for i in 0..n {
        for j in (i + 1)..n {
            if let (Some(pi), Some(pj)) = (parents[i], parents[j]) {
                if pi == pj {
                    for _ in 0..DIRECTORY_PRIOR_WEIGHT {
                        pairs.push((i, j));
                    }
                }
            }
        }
    }

    let labels = super::detect_communities(n, &pairs);
    if labels.is_empty() {
        return Vec::new();
    }

    // Intra-cluster edge counts (deduplicated pairs, not the weighted multiset above) for cohesion.
    let mut members_by_cluster: Vec<Vec<usize>> = Vec::new();
    for (i, &c) in labels.iter().enumerate() {
        if c >= members_by_cluster.len() {
            members_by_cluster.resize(c + 1, Vec::new());
        }
        members_by_cluster[c].push(i);
    }

    let mut intra_edges: Vec<std::collections::HashSet<(usize, usize)>> =
        vec![std::collections::HashSet::new(); members_by_cluster.len()];
    for e in &graph.edges {
        if let (Some(&a), Some(&b)) = (idx.get(e.from.as_str()), idx.get(e.to.as_str())) {
            if a != b && labels[a] == labels[b] {
                let key = if a < b { (a, b) } else { (b, a) };
                intra_edges[labels[a]].insert(key);
            }
        }
    }

    let mut modules: Vec<ComputedModule> = members_by_cluster
        .into_iter()
        .enumerate()
        .map(|(c, member_idxs)| {
            let k = member_idxs.len();
            let e = intra_edges[c].len();
            let cohesion = if k <= 1 {
                1.0
            } else {
                (2.0 * e as f64 / (k as f64 * (k as f64 - 1.0))).clamp(0.0, 1.0)
            };
            ComputedModule {
                module_id: c,
                label: String::new(),
                cohesion,
                members: member_idxs
                    .into_iter()
                    .map(|i| graph.nodes[i].path.clone())
                    .collect(),
            }
        })
        .collect();

    modules.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members.first().cmp(&b.members.first()))
    });
    modules
}

/// One persisted module, read back from `graph_modules` with its members regrouped.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphModule {
    pub module_id: i64,
    pub label: String,
    pub cohesion: f64,
    pub members: Vec<String>,
}

impl Store {
    /// Replace the entire `graph_modules` table with a fresh computation — like
    /// `replace_co_change`, this is a whole-repo recompute (`indexa graph --compute-modules`),
    /// not an incremental update: cluster membership can shift for any file whenever the graph
    /// changes elsewhere, so a partial update would leave stale rows in unrelated modules.
    pub fn replace_graph_modules(&mut self, modules: &[ComputedModule]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM graph_modules", [])?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO graph_modules (module_id, label, cohesion, member_path)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for m in modules {
                for member in &m.members {
                    ins.execute(params![m.module_id as i64, m.label, m.cohesion, member])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All persisted modules, largest first, each with its member paths regrouped.
    pub fn graph_modules(&self) -> Result<Vec<GraphModule>> {
        self.graph_modules_for_scope("")
    }

    /// Persisted modules that have at least one member under `prefix` (empty prefix = all
    /// modules) — each module's FULL member list is still returned, not just the
    /// prefix-matching members, so a scoped view still shows a module's true size/shape.
    pub fn graph_modules_for_scope(&self, prefix: &str) -> Result<Vec<GraphModule>> {
        let pattern = super::search::like_prefix(prefix);
        let mut stmt = self.conn.prepare(
            "SELECT module_id, label, cohesion, member_path FROM graph_modules
              WHERE module_id IN (
                  SELECT DISTINCT module_id FROM graph_modules
                   WHERE member_path LIKE ?1 ESCAPE '\\'
              )
              ORDER BY module_id, member_path",
        )?;
        let rows = stmt.query_map(params![pattern], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;

        let mut by_module: std::collections::BTreeMap<i64, GraphModule> =
            std::collections::BTreeMap::new();
        for row in rows {
            let (module_id, label, cohesion, member_path) = row?;
            by_module
                .entry(module_id)
                .or_insert_with(|| GraphModule {
                    module_id,
                    label,
                    cohesion,
                    members: Vec::new(),
                })
                .members
                .push(member_path);
        }
        let mut out: Vec<GraphModule> = by_module.into_values().collect();
        out.sort_by(|a, b| {
            b.members
                .len()
                .cmp(&a.members.len())
                .then(a.module_id.cmp(&b.module_id))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::types::{CodeGraphEdge, CodeGraphNode};

    fn node(path: &str) -> CodeGraphNode {
        CodeGraphNode {
            path: path.to_owned(),
            out_degree: 0,
            in_degree: 0,
            pagerank: 0.0,
        }
    }

    fn edge(from: &str, to: &str, weight: usize) -> CodeGraphEdge {
        CodeGraphEdge {
            from: from.to_owned(),
            to: to.to_owned(),
            weight,
        }
    }

    #[test]
    fn empty_graph_yields_no_modules() {
        let graph = CodeGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            truncated: false,
        };
        assert!(cluster_with_directory_priors(&graph).is_empty());
    }

    #[test]
    fn two_call_cliques_with_no_shared_directory_split_into_two_modules() {
        let graph = CodeGraph {
            nodes: vec![
                node("/repo/a/one.rs"),
                node("/repo/a/two.rs"),
                node("/repo/b/three.rs"),
                node("/repo/b/four.rs"),
            ],
            edges: vec![
                edge("/repo/a/one.rs", "/repo/a/two.rs", 3),
                edge("/repo/b/three.rs", "/repo/b/four.rs", 3),
            ],
            truncated: false,
        };
        let modules = cluster_with_directory_priors(&graph);
        assert_eq!(modules.len(), 2);
        for m in &modules {
            assert_eq!(m.members.len(), 2);
            assert_eq!(m.cohesion, 1.0);
        }
    }

    #[test]
    fn directory_prior_merges_a_caller_free_sibling_into_its_directory_module() {
        // `helper.rs` has NO call edges at all but shares a directory with the `a` clique —
        // the directory prior should pull it into that module rather than leaving it a
        // same-call-graph-isolated singleton merged arbitrarily.
        let graph = CodeGraph {
            nodes: vec![
                node("/repo/a/one.rs"),
                node("/repo/a/two.rs"),
                node("/repo/a/helper.rs"),
                node("/repo/b/three.rs"),
                node("/repo/b/four.rs"),
            ],
            edges: vec![
                edge("/repo/a/one.rs", "/repo/a/two.rs", 3),
                edge("/repo/b/three.rs", "/repo/b/four.rs", 3),
            ],
            truncated: false,
        };
        let modules = cluster_with_directory_priors(&graph);
        let a_module = modules
            .iter()
            .find(|m| m.members.iter().any(|p| p == "/repo/a/one.rs"))
            .unwrap();
        assert!(a_module.members.contains(&"/repo/a/helper.rs".to_owned()));
    }

    #[test]
    fn modules_are_sorted_largest_first() {
        let graph = CodeGraph {
            nodes: vec![
                node("/repo/big/a.rs"),
                node("/repo/big/b.rs"),
                node("/repo/big/c.rs"),
                node("/repo/small/x.rs"),
            ],
            edges: vec![
                edge("/repo/big/a.rs", "/repo/big/b.rs", 1),
                edge("/repo/big/b.rs", "/repo/big/c.rs", 1),
            ],
            truncated: false,
        };
        let modules = cluster_with_directory_priors(&graph);
        assert!(modules[0].members.len() >= modules.last().unwrap().members.len());
    }
}
