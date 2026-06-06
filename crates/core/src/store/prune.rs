//! Garbage-collect orphaned index rows — chunks/summaries (and their satellite rows)
//! whose `path` has no matching `entries` row. These accumulate when a root is removed or
//! re-pointed and the dangling rows are left behind (Indexa has no FK CASCADE by design —
//! see `store::schema`). `indexa prune` calls these.

use super::Store;
use anyhow::Result;

/// Counts of orphaned index rows (rows whose path has no `entries` row).
#[derive(Debug, Default, Clone, Copy)]
pub struct OrphanCounts {
    pub chunks: u64,
    pub summaries: u64,
}

impl OrphanCounts {
    pub fn is_empty(&self) -> bool {
        self.chunks == 0 && self.summaries == 0
    }
}

impl Store {
    /// Count chunks and summaries whose path has no matching `entries` row.
    pub fn count_orphans(&self) -> Result<OrphanCounts> {
        let chunks: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE entry_path NOT IN (SELECT path FROM entries)",
            [],
            |r| r.get(0),
        )?;
        let summaries: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM summaries WHERE path NOT IN (SELECT path FROM entries)",
            [],
            |r| r.get(0),
        )?;
        Ok(OrphanCounts {
            chunks: chunks as u64,
            summaries: summaries as u64,
        })
    }

    /// Delete orphaned rows (chunks/summaries + their FTS/edges/queue/classification satellites)
    /// whose path has no `entries` row, in one transaction. Returns the counts removed.
    ///
    /// **Guard:** when there are *no* entries at all this is a no-op. A fully entry-less index is
    /// the legitimate `deep`/`summarize`-without-`scan` workflow (entries are optional by design);
    /// without the guard, `NOT IN (empty set)` is true for every row and prune would wipe it.
    pub fn prune_orphans(&mut self) -> Result<OrphanCounts> {
        let entry_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        if entry_count == 0 {
            return Ok(OrphanCounts::default());
        }

        let removed = self.count_orphans()?;
        let tx = self.conn.transaction()?;
        // Satellites first, then the primary tables (order is cosmetic — no FK).
        tx.execute(
            "DELETE FROM chunks_fts WHERE entry_path NOT IN (SELECT path FROM entries)",
            [],
        )?;
        tx.execute(
            "DELETE FROM edges WHERE from_path NOT IN (SELECT path FROM entries)",
            [],
        )?;
        tx.execute(
            "DELETE FROM chunks WHERE entry_path NOT IN (SELECT path FROM entries)",
            [],
        )?;
        tx.execute(
            "DELETE FROM summary_queue WHERE path NOT IN (SELECT path FROM entries)",
            [],
        )?;
        tx.execute(
            "DELETE FROM classifications WHERE path NOT IN (SELECT path FROM entries)",
            [],
        )?;
        tx.execute(
            "DELETE FROM summaries WHERE path NOT IN (SELECT path FROM entries)",
            [],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}
