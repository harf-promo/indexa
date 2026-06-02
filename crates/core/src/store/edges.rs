//! Code-relationship-graph edge writes and queries (the `edges` table).
//!
//! D1: per-file `imports` and `defines` edges.
//! D2: per-file `calls` edges — function/method names called by a file.

use super::{EdgeRecord, Store};
use anyhow::Result;
use rusqlite::params;

impl Store {
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
}
