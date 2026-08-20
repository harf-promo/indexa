//! Co-change edge writes and queries (the `co_change` table, 2.7).
//!
//! Files that historically change together in git history — behavioral coupling
//! invisible to static analysis. Computed offline from `git log --name-only`; readers
//! opt in (`find_related_files_resolved`'s `include_co_change`).

use super::Store;
use anyhow::Result;
use rusqlite::params;

/// One co-change pair with its historical co-commit count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoChangePair {
    pub path: String,
    pub count: i64,
}

/// Canonical `(path_a, path_b)` ordering for the symmetric `co_change` table — lexically
/// smaller first, so a pair is stored once regardless of which file is queried.
fn canonical_pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

impl Store {
    /// Replace the entire `co_change` table with `pairs` — an offline recompute
    /// (`indexa graph --compute-co-change`), not an incremental update: the historical
    /// signal is a whole-repo aggregate, not something a single file's re-`deep` can
    /// partially refresh. `(path_a, path_b, count)` triples; `path_a`/`path_b` need not
    /// already be in canonical order — this normalizes them.
    pub fn replace_co_change(&mut self, pairs: &[(String, String, i64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM co_change", [])?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO co_change (path_a, path_b, count) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path_a, path_b) DO UPDATE SET count = count + excluded.count",
            )?;
            for (a, b, count) in pairs {
                if a == b {
                    continue; // a file can't co-change with itself
                }
                let (pa, pb) = canonical_pair(a, b);
                ins.execute(params![pa, pb, count])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Files that historically co-changed with `path`, heaviest pairing first, capped at
    /// `limit`. Checks both table columns since a pair is stored once in canonical order.
    pub fn co_change_for(&self, path: &str, limit: usize) -> Result<Vec<CoChangePair>> {
        let mut stmt = self.conn.prepare(
            "SELECT CASE WHEN path_a = ?1 THEN path_b ELSE path_a END AS other, count
               FROM co_change
              WHERE path_a = ?1 OR path_b = ?1
              ORDER BY count DESC, other ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![path, limit as i64], |r| {
            Ok(CoChangePair {
                path: r.get(0)?,
                count: r.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}
