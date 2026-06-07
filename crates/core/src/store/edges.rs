//! Code-relationship-graph edge writes and queries (the `edges` table).
//!
//! D1: per-file `imports` and `defines` edges.
//! D2: per-file `calls` edges — function/method names called by a file.

use super::search::like_prefix;
use super::{CodeGraph, CodeGraphEdge, CodeGraphNode, EdgeRecord, RelatedFile, Store};
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;

/// Tarjan's strongly-connected-components, iterative (no recursion → no stack-overflow risk
/// on deep graphs). `adj[v]` lists v's out-neighbors. Returns every SCC (including singletons);
/// the caller keeps those with len > 1 as cycles.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut idx = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    let mut next_index = 0usize;

    for start in 0..n {
        if idx[start] != usize::MAX {
            continue;
        }
        // DFS frames: (node, next-child-pointer).
        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, ci)) = call_stack.last() {
            if ci == 0 {
                idx[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if ci < adj[v].len() {
                let w = adj[v][ci];
                call_stack.last_mut().unwrap().1 += 1;
                if idx[w] == usize::MAX {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(idx[w]);
                }
            } else {
                // Finished v: if it's an SCC root, pop the component.
                if low[v] == idx[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    sccs
}

impl Store {
    /// A `defines` symbol present in more than this many files is treated as a generic
    /// name (`new`/`from`/`default`) and excluded from [`Self::code_graph`] — it bounds the
    /// JOIN's worst case and removes low-signal noise.
    const CODE_GRAPH_COMMON_SYMBOL_CAP: i64 = 25;

    /// Replace every edge originating at each file in the batch (delete-by-`from_path`
    /// then insert), mirroring [`upsert_chunks`](Self::upsert_chunks) so a re-`deep` of a
    /// file refreshes its graph rather than accumulating stale edges. `INSERT OR IGNORE`
    /// collapses duplicates against the composite primary key.
    pub fn upsert_edges(&mut self, edges: &[EdgeRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut del = tx.prepare_cached("DELETE FROM edges WHERE from_path = ?1")?;
            let mut cleared: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for e in edges {
                if cleared.insert(e.from_path.as_str()) {
                    del.execute(params![e.from_path])?;
                }
            }
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO edges (from_path, kind, to_ref) VALUES (?1, ?2, ?3)",
            )?;
            for e in edges {
                ins.execute(params![e.from_path, e.kind, e.to_ref])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All edges originating at `from_path` (the file's imports and the symbols it
    /// defines), ordered by kind then target for stable output.
    pub fn edges_from(&self, from_path: &str) -> Result<Vec<EdgeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_path, kind, to_ref FROM edges WHERE from_path = ?1 ORDER BY kind, to_ref",
        )?;
        let rows = stmt.query_map(params![from_path], |r| {
            Ok(EdgeRecord {
                from_path: r.get(0)?,
                kind: r.get(1)?,
                to_ref: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Reverse lookup: the distinct files that have a `kind` edge to `to_ref` — e.g. who
    /// imports a module (`kind="imports"`) or who defines a symbol (`kind="defines"`).
    /// Sorted for stable output.
    pub fn edges_to(&self, kind: &str, to_ref: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT from_path FROM edges WHERE kind = ?1 AND to_ref = ?2 ORDER BY from_path",
        )?;
        let rows = stmt.query_map(params![kind, to_ref], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// D2 — files that contain a `calls` edge to `symbol` (direct callers), capped at
    /// `limit`. The match is on the bare symbol name, case-sensitive.
    pub fn who_calls(&self, symbol: &str, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT from_path FROM edges
              WHERE kind = 'calls' AND to_ref = ?1
              ORDER BY from_path LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![symbol, limit as i64], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Files related to `path` through the call graph, ranked by the number of shared
    /// call→define symbols (the relation strength). A file is related when `path` calls a
    /// symbol it defines (a dependency) **or** it calls a symbol `path` defines (a
    /// dependent) — both directions are merged. Over-common symbols (defined in more than
    /// [`Self::CODE_GRAPH_COMMON_SYMBOL_CAP`] files) are excluded as noise, exactly like
    /// `code_graph`. Bare-name matched, so it inherits the same approximate-ness.
    pub fn find_related_files(&self, path: &str, limit: usize) -> Result<Vec<RelatedFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT other, SUM(w) AS shared FROM (
                 SELECT d.from_path AS other, COUNT(DISTINCT c.to_ref) AS w
                   FROM edges c
                   JOIN edges d ON d.kind = 'defines' AND d.to_ref = c.to_ref
                  WHERE c.kind = 'calls' AND c.from_path = ?1 AND d.from_path <> ?1
                    AND c.to_ref NOT IN (
                        SELECT to_ref FROM edges WHERE kind = 'defines'
                         GROUP BY to_ref HAVING COUNT(DISTINCT from_path) > ?3)
                  GROUP BY d.from_path
                 UNION ALL
                 SELECT c.from_path AS other, COUNT(DISTINCT c.to_ref) AS w
                   FROM edges c
                   JOIN edges d ON d.kind = 'defines' AND d.to_ref = c.to_ref
                  WHERE c.kind = 'calls' AND d.from_path = ?1 AND c.from_path <> ?1
                    AND c.to_ref NOT IN (
                        SELECT to_ref FROM edges WHERE kind = 'defines'
                         GROUP BY to_ref HAVING COUNT(DISTINCT from_path) > ?3)
                  GROUP BY c.from_path
             )
             GROUP BY other
             ORDER BY shared DESC, other ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![path, limit as i64, Self::CODE_GRAPH_COMMON_SYMBOL_CAP],
            |r| {
                Ok(RelatedFile {
                    path: r.get(0)?,
                    shared: r.get::<_, i64>(1)? as usize,
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Detect dependency cycles in the file-to-file call graph under `prefix` (Tarjan SCC).
    /// Returns each strongly-connected component of size > 1 (a genuine cycle) as a sorted
    /// list of file paths, largest cycle first. Runs over the same edges as `code_graph`
    /// (so it shares the bare-name caveat); `max_edges` bounds the graph it analyzes.
    pub fn find_cycles(&self, prefix: &str, max_edges: usize) -> Result<Vec<Vec<String>>> {
        let graph = self.code_graph(prefix, max_edges, false)?;
        // Index nodes; build adjacency (caller → callee).
        let nodes: Vec<&str> = graph.nodes.iter().map(|n| n.path.as_str()).collect();
        let idx: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for e in &graph.edges {
            if let (Some(&a), Some(&b)) = (idx.get(e.from.as_str()), idx.get(e.to.as_str())) {
                adj[a].push(b);
            }
        }
        let mut sccs = tarjan_scc(&adj);
        // Keep only true cycles (SCC size > 1), map indices back to paths, sort for stability.
        let mut cycles: Vec<Vec<String>> = sccs
            .drain(..)
            .filter(|c| c.len() > 1)
            .map(|c| {
                let mut paths: Vec<String> = c.into_iter().map(|i| nodes[i].to_owned()).collect();
                paths.sort();
                paths
            })
            .collect();
        cycles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        Ok(cycles)
    }

    /// How many distinct files `define` a symbol of this exact name. Used to **annotate**
    /// `who_calls` results: a name defined in >1 file is ambiguous, so the caller list may
    /// conflate references to different definitions (bare-name matching can't tell them
    /// apart without an import resolver). `0` means the symbol isn't defined in the index.
    pub fn defines_count(&self, symbol: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT from_path) FROM edges WHERE kind = 'defines' AND to_ref = ?1",
            params![symbol],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// D2 — 1-hop blast radius for `symbol`: direct callers **plus** files that call any
    /// symbol defined in one of those callers. Gives a conservative "what breaks if I
    /// change this?" set without full recursive name resolution. Capped at `limit`.
    ///
    /// `strict` tightens the **transitive** hop: a caller's exported symbol is only followed
    /// when that name is defined in exactly one file, so an ambiguous common name (defined
    /// in several files) no longer drags unrelated callers into the radius. The direct
    /// callers of `symbol` itself can't be tightened this way (the input is a bare name with
    /// no definer to disambiguate against) — see `who_calls` + `defines_count`. `strict` is a
    /// precision filter on names, **not** import resolution.
    pub fn blast_radius(&self, symbol: &str, limit: usize, strict: bool) -> Result<Vec<String>> {
        // In strict mode, restrict the followed exports to symbols with a unique definition.
        let strict_clause = if strict {
            "AND to_ref NOT IN (
                 SELECT to_ref FROM edges WHERE kind = 'defines'
                  GROUP BY to_ref HAVING COUNT(DISTINCT from_path) > 1
             )"
        } else {
            ""
        };
        let sql = format!(
            "WITH direct_callers AS (
                 SELECT DISTINCT from_path FROM edges
                  WHERE kind = 'calls' AND to_ref = ?1
             ),
             caller_exports AS (
                 SELECT DISTINCT to_ref FROM edges
                  WHERE kind = 'defines'
                    AND from_path IN (SELECT from_path FROM direct_callers)
                    {strict_clause}
             ),
             transitive_callers AS (
                 SELECT DISTINCT from_path FROM edges
                  WHERE kind = 'calls'
                    AND to_ref IN (SELECT to_ref FROM caller_exports)
             )
             SELECT from_path FROM direct_callers
             UNION
             SELECT from_path FROM transitive_callers
             ORDER BY from_path
             LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![symbol, limit as i64], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Build a file-to-file **call graph** for files under `prefix` (v0.18 signature graph).
    ///
    /// An edge `A → B` exists when file A has a `calls` edge to a symbol that file B `defines`
    /// — i.e. A calls a function B provides. `weight` is the number of distinct shared symbols.
    /// Both endpoints must be under `prefix`. Self-edges are excluded. Capped at `max_edges`
    /// (heaviest edges first); `truncated` is set when the cap is hit.
    ///
    /// Like all D2 call data, matching is on the **bare symbol name** (case-sensitive), so a
    /// call to `parse` links to any in-scope file defining `parse`. See `docs/methodology.md`.
    ///
    /// Symbols defined in more than [`Self::CODE_GRAPH_COMMON_SYMBOL_CAP`] files are excluded:
    /// they are generic names (`new`, `from`, `default`, `fmt`) whose caller×definer pairings
    /// both bloat the JOIN on a whole-disk index and produce noise rather than signal.
    ///
    /// `strict` lowers that exclusion threshold to **1** — only symbols with a *unique*
    /// definition produce edges, so name collisions (the source of bare-name false positives)
    /// can't create spurious `A → B` links. It is a precision filter on names, **not** import
    /// resolution: a locally-shadowed name uniquely defined elsewhere can still mislink, and a
    /// genuinely-shared helper is dropped. `strict` trades recall for precision; `false`
    /// (default) keeps the historical >25-file behavior so PageRank / Map node sizing are
    /// unchanged.
    pub fn code_graph(&self, prefix: &str, max_edges: usize, strict: bool) -> Result<CodeGraph> {
        // strict → keep only uniquely-defined symbols (defined in exactly 1 file);
        // fuzzy → the historical "common name" noise cap.
        let common_cap = if strict {
            1
        } else {
            Self::CODE_GRAPH_COMMON_SYMBOL_CAP
        };
        // Normalize to a directory prefix so `/a/proj` doesn't also match `/a/projector`.
        // `/` (whole disk) is left as-is → matches everything.
        let dir = if prefix == "/" || prefix.ends_with('/') {
            prefix.to_owned()
        } else {
            format!("{prefix}/")
        };
        let pattern = like_prefix(&dir);
        // +1 so we can detect truncation without a second COUNT query.
        let fetch = max_edges.saturating_add(1) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT c.from_path AS caller, d.from_path AS callee,
                    COUNT(DISTINCT c.to_ref) AS weight
               FROM edges c
               JOIN edges d ON d.kind = 'defines' AND d.to_ref = c.to_ref
              WHERE c.kind = 'calls'
                AND c.from_path LIKE ?1 ESCAPE '\\'
                AND d.from_path LIKE ?1 ESCAPE '\\'
                AND c.from_path <> d.from_path
                AND c.to_ref NOT IN (
                    SELECT to_ref FROM edges WHERE kind = 'defines'
                     GROUP BY to_ref HAVING COUNT(DISTINCT from_path) > ?3
                )
              GROUP BY c.from_path, d.from_path
              ORDER BY weight DESC, caller, callee
              LIMIT ?2",
        )?;
        let raw: Vec<(String, String, i64)> = stmt
            .query_map(params![pattern, fetch, common_cap], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let truncated = raw.len() > max_edges;
        let edges_raw = if truncated {
            &raw[..max_edges]
        } else {
            &raw[..]
        };

        // Accumulate degree counts per node.
        let mut out_deg: HashMap<&str, usize> = HashMap::new();
        let mut in_deg: HashMap<&str, usize> = HashMap::new();
        let mut edges = Vec::with_capacity(edges_raw.len());
        for (from, to, weight) in edges_raw {
            *out_deg.entry(from.as_str()).or_insert(0) += 1;
            *in_deg.entry(to.as_str()).or_insert(0) += 1;
            edges.push(CodeGraphEdge {
                from: from.clone(),
                to: to.clone(),
                weight: *weight as usize,
            });
        }

        // Node set = every path that appears as a caller or callee (sorted for
        // stable ordering, then indexed so PageRank can run on integer ids).
        let mut paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (from, to, _) in edges_raw {
            paths.insert(from.as_str());
            paths.insert(to.as_str());
        }
        let node_paths: Vec<&str> = paths.into_iter().collect();
        let idx: HashMap<&str, usize> = node_paths
            .iter()
            .enumerate()
            .map(|(i, &p)| (p, i))
            .collect();

        // Weighted PageRank over the displayed (post-cap) edge set: rank flows
        // caller → callee, so hub files called by many score highest.
        let pr_edges: Vec<(usize, usize, f64)> = edges_raw
            .iter()
            .map(|(from, to, weight)| (idx[from.as_str()], idx[to.as_str()], *weight as f64))
            .collect();
        let scores = super::pagerank::pagerank(node_paths.len(), &pr_edges);

        let nodes = node_paths
            .iter()
            .enumerate()
            .map(|(i, &p)| CodeGraphNode {
                path: p.to_owned(),
                out_degree: out_deg.get(p).copied().unwrap_or(0),
                in_degree: in_deg.get(p).copied().unwrap_or(0),
                pagerank: scores[i],
            })
            .collect();

        Ok(CodeGraph {
            nodes,
            edges,
            truncated,
        })
    }
}
