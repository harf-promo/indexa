//! Code-relationship-graph edge writes and queries (the `edges` table).
//!
//! D1: per-file `imports` and `defines` edges.
//! D2: per-file `calls` edges — function/method names called by a file.

use super::search::like_prefix;
use super::{CodeGraph, CodeGraphEdge, CodeGraphNode, EdgeRecord, Store};
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;

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

    /// D2 — 1-hop blast radius for `symbol`: direct callers **plus** files that call any
    /// symbol defined in one of those callers. Gives a conservative "what breaks if I
    /// change this?" set without full recursive name resolution. Capped at `limit`.
    pub fn blast_radius(&self, symbol: &str, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "WITH direct_callers AS (
                 SELECT DISTINCT from_path FROM edges
                  WHERE kind = 'calls' AND to_ref = ?1
             ),
             caller_exports AS (
                 SELECT DISTINCT to_ref FROM edges
                  WHERE kind = 'defines'
                    AND from_path IN (SELECT from_path FROM direct_callers)
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
             LIMIT ?2",
        )?;
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
    pub fn code_graph(&self, prefix: &str, max_edges: usize) -> Result<CodeGraph> {
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
            .query_map(
                params![pattern, fetch, Self::CODE_GRAPH_COMMON_SYMBOL_CAP],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
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

        // Node set = every path that appears as a caller or callee.
        let mut paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (from, to, _) in edges_raw {
            paths.insert(from.as_str());
            paths.insert(to.as_str());
        }
        let nodes = paths
            .into_iter()
            .map(|p| CodeGraphNode {
                path: p.to_owned(),
                out_degree: out_deg.get(p).copied().unwrap_or(0),
                in_degree: in_deg.get(p).copied().unwrap_or(0),
            })
            .collect();

        Ok(CodeGraph {
            nodes,
            edges,
            truncated,
        })
    }
}
