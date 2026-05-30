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
