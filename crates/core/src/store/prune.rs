//! Garbage-collect orphaned index rows — chunks/summaries (and their satellite rows)
//! whose `path` has no matching `entries` row. These accumulate when a root is removed or
//! re-pointed and the dangling rows are left behind (Indexa has no FK CASCADE by design —
//! see `store::schema`). `indexa prune` calls these.

use super::Store;
use anyhow::Result;

/// `(table, path column)` for every single-column entry-keyed child table the sweep clears —
/// the same set `entries.rs`'s `ENTRY_CHILD_TABLES` deletes when an entry goes away, so pruning
/// leaves exactly what an explicit delete would have left. `co_change` is keyed on a PAIR of
/// columns and handled separately (see [`CO_CHANGE_ORPHAN`]). Satellites first, primary tables
/// last (order is cosmetic — there is no FK). Locked by `prune_clears_every_entry_child_table`,
/// which seeds every child table via `seed_full_entry` and checks `orphan_rows_for` hits zero.
const ORPHAN_TABLES: &[(&str, &str)] = &[
    ("chunks_fts", "entry_path"),
    ("edges", "from_path"),
    ("symbols", "path"),
    ("note_anchors", "note_path"),
    ("chunks", "entry_path"),
    ("summary_queue", "path"),
    ("classifications", "path"),
    ("directory_apps", "path"),
    ("summaries", "path"),
];

/// A `co_change` pair is orphaned when EITHER side has no `entries` row — mirroring the
/// entry-delete paths, which drop a pair as soon as either of its files is deleted.
const CO_CHANGE_ORPHAN: &str = "path_a NOT IN (SELECT path FROM entries)
     OR path_b NOT IN (SELECT path FROM entries)";

/// Counts of orphaned index rows (rows whose path has no `entries` row).
#[derive(Debug, Default, Clone, Copy)]
pub struct OrphanCounts {
    pub chunks: u64,
    pub summaries: u64,
    /// Dead summary-queue rows (e.g. build-artifact paths that can never summarize).
    pub queue: u64,
    /// Orphaned classification rows.
    pub classifications: u64,
    /// Orphaned directory-app detection rows.
    pub directory_apps: u64,
    /// Orphaned code-graph symbol rows (written by the same `deep` pass as `edges` — `symbols`
    /// had been left out of the sweep, so a pruned file's stale symbols lingered and
    /// `symbol_context` kept citing them).
    pub symbols: u64,
    /// Orphaned code-graph edge rows (keyed on `from_path`). Always deleted by the sweep; now
    /// also counted, so the report matches what was removed.
    pub edges: u64,
    /// Orphaned note-anchor rows (a note whose own file has no `entries` row).
    pub note_anchors: u64,
    /// Orphaned co-change pairs (either side has no `entries` row).
    pub co_change: u64,
}

impl OrphanCounts {
    pub fn is_empty(&self) -> bool {
        self.chunks == 0
            && self.summaries == 0
            && self.queue == 0
            && self.classifications == 0
            && self.directory_apps == 0
            && self.symbols == 0
            && self.edges == 0
            && self.note_anchors == 0
            && self.co_change == 0
    }

    /// Every count as one human-readable list, e.g. `"3 orphaned chunk(s), …, and 1 co-change
    /// pair(s)"`. The single wording shared by `indexa prune` (dry-run and real) and the MCP
    /// `prune` tool, so a field added here is reported everywhere instead of being deleted
    /// silently (`edges`/`note_anchors`/`co_change` were swept but never printed).
    pub fn summary(&self) -> String {
        format!(
            "{} orphaned chunk(s), {} stale queue row(s), {} summary(ies), {} classification(s), \
             {} app detection(s), {} symbol row(s), {} edge row(s), {} note anchor(s), and {} \
             co-change pair(s)",
            self.chunks,
            self.queue,
            self.summaries,
            self.classifications,
            self.directory_apps,
            self.symbols,
            self.edges,
            self.note_anchors,
            self.co_change
        )
    }
}

impl Store {
    /// Count rows in every entry-keyed child table whose path has no matching `entries` row.
    /// (Mirrors exactly what [`prune_orphans`](Self::prune_orphans) deletes, so the report is
    /// honest; `chunks_fts` is a satellite of `chunks` and is not reported separately.)
    pub fn count_orphans(&self) -> Result<OrphanCounts> {
        let count = |table: &str, col: &str| -> Result<u64> {
            let n: i64 = self.conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE {col} NOT IN (SELECT path FROM entries)"
                ),
                [],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        };
        let co_change: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM co_change WHERE {CO_CHANGE_ORPHAN}"),
            [],
            |r| r.get(0),
        )?;
        Ok(OrphanCounts {
            chunks: count("chunks", "entry_path")?,
            summaries: count("summaries", "path")?,
            queue: count("summary_queue", "path")?,
            classifications: count("classifications", "path")?,
            directory_apps: count("directory_apps", "path")?,
            symbols: count("symbols", "path")?,
            edges: count("edges", "from_path")?,
            note_anchors: count("note_anchors", "note_path")?,
            co_change: co_change as u64,
        })
    }

    /// Delete orphaned rows (chunks/summaries + their FTS/edges/symbols/note-anchor/co-change/
    /// queue/classification/app satellites) whose path has no `entries` row, in one transaction.
    /// Returns the counts removed.
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
        for &(table, col) in ORPHAN_TABLES {
            tx.execute(
                &format!("DELETE FROM {table} WHERE {col} NOT IN (SELECT path FROM entries)"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM co_change WHERE {CO_CHANGE_ORPHAN}"),
            [],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}
