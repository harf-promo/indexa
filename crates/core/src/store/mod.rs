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
//! - [`decisions`] — Decision Ledger reads/writes (v0.22).
//! - [`packs`] — Context Pack CRUD (v0.9).
//! - [`weights`] — importance weight CRUD + search boost (v0.8).
//! - [`insights`] — duplicate/stale/diff analysis (v0.10).
//! - [`usage`] — token-savings telemetry (v0.23; the counterfactual definition lives there).
//! - [`types`] — the public record structs.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

mod ann;
mod category_edges;
mod chunks;
mod classify;
mod decisions;
mod dir_apps;
mod edges;
mod entries;
mod insights;
mod packs;
mod pagerank;
mod prune;
mod queue;
mod saved;
mod schema;
mod search;
mod semantic_edges;
mod sessions;
mod summaries;
mod types;
mod usage;
mod weights;

#[cfg(test)]
mod tests;

// Re-export every public record type so external paths (`indexa_core::store::*`)
// are unchanged by the split.
pub use ann::AnnIndex;
pub use dir_apps::DetectedApp;
pub use edges::{
    BlastRadius, ResolutionTier, ResolvedCaller, ResolvedRelatedFile, ScopedCodeGraph,
    BARE_NAME_CAVEAT,
};
pub use entries::CoverageEntry;
pub use insights::{DuplicateCluster, LanguageStat, LargestEntry, StaleEntry, WeeklyDiff};
pub use prune::OrphanCounts;
pub use saved::SavedQuery;
pub use sessions::ConversationTurn;
// Stub-chunk filter for retrieval (excludes content-free "File: <name>" image/binary
// placeholders); the query crate's `retrieve()` guard reuses it.
pub use search::is_stub_chunk;
pub use types::{
    chunk_content_hash, ChunkRecord, ClassificationRecord, CodeGraph, CodeGraphEdge, CodeGraphNode,
    DecisionRecord, EdgeRecord, EntryInfo, FailedQueueItem, HealthStats, NewDecision, PackRecord,
    QueueItem, QueueStats, RegionSummary, RelatedFile, SearchHit, SummaryRecord, TreeNode,
    WeightRecord,
};
pub use usage::{UsageSummary, USAGE_WEEK_SECS};

// `abstract_from` is part of the public surface (used by `indexa_core::store::abstract_from`).
// The source-hash helpers (incremental re-summarize) live beside the summary writes
// they gate: the query crate computes them, the store persists them.
pub use summaries::{abstract_from, dir_source_hash, file_source_hash};

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
