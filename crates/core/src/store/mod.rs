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
//! - [`types`] — the public record structs.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

mod chunks;
mod classify;
mod entries;
mod queue;
mod schema;
mod search;
mod summaries;
mod types;

#[cfg(test)]
mod tests;

// Re-export every public record type so external paths (`indexa_core::store::*`)
// are unchanged by the split.
pub use types::{
    ChunkRecord, ClassificationRecord, FailedQueueItem, QueueItem, QueueStats, RegionSummary,
    SearchHit, SummaryRecord, TreeNode,
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
        let store = Self {
            conn,
            db_path: path.to_path_buf(),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
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
}
