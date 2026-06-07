//! SQLite-backed index store.
//!
//! The implementation is split across several files, each contributing methods to
//! the single `impl Store` via Rust's multiple-`impl`-block support:
//! - [`schema`] — table/index DDL and migrations (`init_schema`).
//! - [`entries`] — surface-scan entry CRUD and subtree reconciliation.
//! - [`chunks`] — deep-scan chunk writes and chunk-level queries.
//! - [`search`] — hybrid/cosine search and the FTS/embedding helpers.
//! - [`summaries`] — hierarchical summary reads/writes and tree shaping.
//! - [`queue`] — the background summarization queue.
//! - [`classify`] — semantic-classification reads/writes (Smart classification).
//! - [`packs`] — Context Pack CRUD (v0.9).
//! - [`weights`] — importance weight CRUD + search boost (v0.8).
//! - [`insights`] — duplicate/stale/diff analysis (v0.10).
//! - [`types`] — the public record structs.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

mod ann;
mod chunks;
mod classify;
mod edges;
mod entries;
mod insights;
mod packs;
mod pagerank;
mod prune;
mod queue;
mod schema;
mod search;
mod summaries;
mod types;
mod weights;

#[cfg(test)]
mod tests;

// Re-export every public record type so external paths (`indexa_core::store::*`)
// are unchanged by the split.
pub use ann::AnnIndex;
pub use entries::CoverageEntry;
pub use insights::{DuplicateCluster, LanguageStat, LargestEntry, StaleEntry, WeeklyDiff};
pub use prune::OrphanCounts;
pub use types::{
    ChunkRecord, ClassificationRecord, CodeGraph, CodeGraphEdge, CodeGraphNode, EdgeRecord,
    FailedQueueItem, PackRecord, QueueItem, QueueStats, RegionSummary, SearchHit, SummaryRecord,
    TreeNode, WeightRecord,
};

// `abstract_from` is part of the public surface (used by `indexa_core::store::abstract_from`).
pub use summaries::abstract_from;

pub struct Store {
    conn: Connection,
    db_path: PathBuf,
}

impl Store {
    /// Open (or create) the index database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating index directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening index at {}", path.display()))?;
        // Set the busy timeout BEFORE any SQL (init_schema sets it again via PRAGMA, but the
        // first statements there — journal_mode=WAL, the AUTOINCREMENT migration — can contend
        // when worker + serve open the same DB at once; without an already-armed timeout that
        // surfaces as an immediate "database is locked" (notably on Windows). 5s matches the
        // PRAGMA.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Self {
            conn,
            db_path: path.to_path_buf(),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut store = Self {
            conn,
            db_path: PathBuf::from(":memory:"),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Path to the on-disk database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Raw connection access for diagnostic / doctor tooling.
    /// Prefer dedicated store methods for all non-diagnostic use.
    pub fn db_connection(&self) -> &Connection {
        &self.conn
    }
}
