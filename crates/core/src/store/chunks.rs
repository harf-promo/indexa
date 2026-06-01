//! Deep-scan chunk writes and chunk-level queries.

use super::entries::delete_chunks_under_prefix;
use super::search::{embedding_to_blob, like_prefix};
use super::{ChunkRecord, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

impl Store {
    /// Returns true if the file at `path` already has chunks whose `indexed_at`
    /// timestamp is >= the file's recorded `modified_s` (mtime).  When true,
    /// `cmd_deep` can skip re-parsing and re-embedding the file.
    pub fn chunks_are_current(&self, path: &str) -> Result<bool> {
        let current: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0
             FROM chunks c
             JOIN entries e ON c.entry_path = e.path
             WHERE e.path = ?1
               AND e.modified_s IS NOT NULL
               AND c.indexed_at >= e.modified_s",
            params![path],
            |r| r.get(0),
        )?;
        Ok(current)
    }

    /// Like [`chunks_are_current`](Self::chunks_are_current), but compares chunks
    /// against a caller-supplied mtime (Unix seconds) instead of the stored
    /// `entries.modified_s`.
    ///
    /// `cmd_deep` walks files fresh from disk and can run *without* a preceding
    /// `scan`, so the DB's recorded `modified_s` may be stale — comparing against
    /// the live on-disk mtime ensures an edited file is re-embedded rather than
    /// skipped. No `entries` join, so it also holds for a file with no entries row.
    pub fn chunks_current_for_mtime(&self, path: &str, mtime_secs: i64) -> Result<bool> {
        let current: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0
             FROM chunks
             WHERE entry_path = ?1
               AND indexed_at >= ?2",
            params![path, mtime_secs],
            |r| r.get(0),
        )?;
        Ok(current)
    }

    // ── Deep-scan writes ──────────────────────────────────────────────────────

    /// Re-index a batch of chunks (text + optional embedding), making the operation
    /// idempotent per `entry_path`.
    ///
    /// Every chunk + FTS row for each `entry_path` in the batch is cleared first, then the
    /// new chunks are inserted. This avoids two bugs in the old `INSERT OR REPLACE` approach:
    /// on a `UNIQUE(entry_path, seq)` conflict, REPLACE deleted the old chunk and inserted a
    /// new one with a *fresh* rowid, so the follow-up `DELETE FROM chunks_fts WHERE
    /// chunk_id = last_insert_rowid()` matched the *new* id and orphaned the old FTS row
    /// (unbounded FTS bloat + skewed BM25); and re-indexing a file that had *shrunk* left the
    /// stale higher-`seq` chunks (and their FTS rows) behind as phantom search hits. Clearing
    /// by `entry_path` and doing a plain INSERT fixes both at once.
    pub fn upsert_chunks(&mut self, chunks: &[ChunkRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            // 1. Clear existing chunks + FTS rows for each distinct entry_path in the batch.
            let mut del_fts = tx.prepare_cached("DELETE FROM chunks_fts WHERE entry_path = ?1")?;
            let mut del_chunks = tx.prepare_cached("DELETE FROM chunks WHERE entry_path = ?1")?;
            let mut cleared: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for c in chunks {
                if cleared.insert(c.entry_path.as_str()) {
                    del_fts.execute(params![c.entry_path])?;
                    del_chunks.execute(params![c.entry_path])?;
                }
            }

            // 2. Insert the new chunk set, keeping FTS5 in sync on the fresh rowid.
            let mut stmt = tx.prepare_cached(
                "INSERT INTO chunks
                 (entry_path, seq, heading, text, language, embedding, embed_model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut fts_ins = tx.prepare_cached(
                "INSERT INTO chunks_fts(text, heading, entry_path, chunk_id)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;

            for c in chunks {
                let embedding_blob = c.embedding.as_deref().map(embedding_to_blob);

                stmt.execute(params![
                    c.entry_path,
                    c.seq as i64,
                    c.heading,
                    c.text,
                    c.language,
                    embedding_blob,
                    c.embed_model,
                ])?;

                let rowid = tx.last_insert_rowid();
                fts_ins.execute(params![c.text, c.heading, c.entry_path, rowid.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete all chunks (and their FTS5 entries) for a given file path.
    /// Called when a watched file is deleted.
    pub fn delete_chunks_for(&mut self, entry_path: &str) -> Result<()> {
        // Remove from FTS5 first (trigger-less, manual delete).
        self.conn.execute(
            "DELETE FROM chunks_fts WHERE entry_path = ?1",
            rusqlite::params![entry_path],
        )?;
        self.conn.execute(
            "DELETE FROM chunks WHERE entry_path = ?1",
            rusqlite::params![entry_path],
        )?;
        // Drop the file's code-graph edges too, so a removed file leaves no orphan edges.
        self.conn.execute(
            "DELETE FROM edges WHERE from_path = ?1",
            rusqlite::params![entry_path],
        )?;
        Ok(())
    }

    /// Delete chunks for every file whose path is under `prefix`.
    pub fn delete_chunks_for_subtree(&mut self, prefix: &str) -> Result<usize> {
        let pattern = like_prefix(prefix);
        let tx = self.conn.transaction()?;
        let n = delete_chunks_under_prefix(&tx, &pattern)?;
        tx.commit()?;
        Ok(n)
    }

    /// Count of chunks that have an embedding stored.
    pub fn embedded_chunk_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE embedding IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Unix timestamp of the most recently indexed chunk, if any.
    pub fn last_indexed_at(&self) -> Result<Option<i64>> {
        let ts: Option<i64> =
            self.conn
                .query_row("SELECT MAX(indexed_at) FROM chunks", [], |r| r.get(0))?;
        Ok(ts)
    }

    /// Count of indexed chunks.
    pub fn chunk_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Largest chunk id, or 0 when empty. With AUTOINCREMENT this is monotonic and never
    /// repeats, so `(chunk_count, max_chunk_id)` is a robust change watermark: any insert
    /// bumps the max, any delete changes the count — including an in-place edit that keeps
    /// the count but reinserts at a fresh id. Used to decide when to rebuild the ANN index.
    pub fn max_chunk_id(&self) -> Result<i64> {
        let id: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM chunks", [], |r| r.get(0))?;
        Ok(id)
    }

    /// Text of the first chunk for a given file path (used as description input).
    pub fn first_chunk_text(&self, entry_path: &str) -> Result<Option<String>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT text FROM chunks WHERE entry_path = ?1 ORDER BY seq LIMIT 1",
                params![entry_path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(text)
    }
}
