//! Deep-scan chunk writes and chunk-level queries.

use super::entries::delete_chunks_under_prefix;
use super::search::{blob_to_embedding, embedding_to_blob};
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
        // "Current" requires the file's chunks to be at least as new as its mtime AND every
        // chunk to carry an embedding (`COUNT(*) = COUNT(embedding)` — COUNT skips NULLs). A
        // chunk stored without a vector — e.g. an embed failure during a broken-Ollama run —
        // must NOT count as current, or `deep` would skip the file forever and the chunk would
        // never get a vector, degrading dense search invisibly. Treating "missing embedding" as
        // not-current lets a plain re-run of `deep` self-heal a partially-embedded index.
        let current: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 AND COUNT(*) = COUNT(embedding)
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
                 (entry_path, seq, heading, text, language, embedding, embed_model, content_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                    c.content_hash,
                ])?;

                let rowid = tx.last_insert_rowid();
                fts_ins.execute(params![c.text, c.heading, c.entry_path, rowid.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns a map of `content_hash → embedding` for chunks already stored under
    /// `entry_path`.  Only rows where BOTH `content_hash` and `embedding` are non-NULL
    /// are returned — a NULL embedding means the previous deep run failed to embed that
    /// chunk, so it must not be reused.
    ///
    /// Used by the deep-scan path to skip re-embedding chunks whose source text (and
    /// therefore hash) has not changed since the last run.
    pub fn cached_embeddings_by_hash(
        &self,
        entry_path: &str,
    ) -> Result<std::collections::HashMap<String, Vec<f32>>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT content_hash, embedding FROM chunks
              WHERE entry_path = ?1
                AND content_hash IS NOT NULL
                AND embedding IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![entry_path], |r| {
            let hash: String = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            Ok((hash, blob))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (hash, blob) = row?;
            map.insert(hash, blob_to_embedding(&blob));
        }
        Ok(map)
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

    /// Delete chunks for the file at `prefix` and every file strictly under it. Uses the same
    /// exact-or-`prefix/%` matching as `delete_subtree`, so a sibling sharing the string prefix
    /// (`/proj` vs `/projector`) is never touched.
    pub fn delete_chunks_for_subtree(&mut self, prefix: &str) -> Result<usize> {
        let (exact, child) = super::entries::subtree_match(prefix);
        let tx = self.conn.transaction()?;
        let n = delete_chunks_under_prefix(&tx, &exact, &child)?;
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

    /// Newest chunk `indexed_at` among files under `root` (prefix match), or `None` when
    /// nothing under the root is deep-indexed. Drives auto-reindex staleness decisions.
    pub fn last_indexed_at_for_root(&self, root: &str) -> Result<Option<i64>> {
        // Normalize to a directory prefix so `/a/proj` doesn't also match `/a/projector`.
        let dir = if root == "/" || root.ends_with('/') {
            root.to_owned()
        } else {
            format!("{root}/")
        };
        let pattern = super::search::like_prefix(&dir);
        let ts: Option<i64> = self.conn.query_row(
            "SELECT MAX(indexed_at) FROM chunks WHERE entry_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
            |r| r.get(0),
        )?;
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

    /// Files whose chunks carry NO language tag, with their chunk counts,
    /// largest first. Feeds the ledger's language-fallback detector: a file
    /// with several untagged chunks parsed as plain text even though its
    /// extension suggests code (the extension filter happens in Rust — SQL
    /// can't map extensions to languages).
    pub fn unlabeled_chunk_files(
        &self,
        min_chunks: i64,
        limit: usize,
    ) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT entry_path, COUNT(*) AS n FROM chunks
              WHERE language IS NULL
              GROUP BY entry_path
             HAVING COUNT(*) >= ?1
              ORDER BY n DESC, entry_path
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![min_chunks, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Tag every chunk of `entry_path` with `language` (the ledger's language
    /// answer projection). Idempotent; returns rows updated. A later re-deep
    /// rewrites the chunks (language NULL again) — the detector re-applies the
    /// standing answer instead of re-asking.
    pub fn set_chunks_language(&mut self, entry_path: &str, language: &str) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE chunks SET language = ?2 WHERE entry_path = ?1",
                params![entry_path, language],
            )
            .map_err(Into::into)
    }

    /// Fetch stored embeddings for a set of chunk ids.
    ///
    /// Returns a map of `chunk_id → embedding`. Ids with no stored embedding (or an
    /// invalid blob) are silently omitted so callers can fail-open if some vectors are
    /// missing. Used by the MMR re-ranking pass in `retrieve()` (v0.42).
    pub fn embeddings_for_chunks(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<f32>>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // One batched `IN (…)` instead of N `query_row`s (mirrors `paths_for_ids`). `ids` is the
        // MMR candidate pool — bounded by the retrieval limit — so it stays under SQLite's variable
        // cap. Missing/invalid blobs are silently skipped (fail-open), same as the old loop; the
        // result map is identical (HashMap, order-independent; duplicate input ids collapse).
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT id, embedding FROM chunks WHERE id IN ({placeholders})");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut map = std::collections::HashMap::with_capacity(ids.len());
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
        })?;
        for row in rows {
            let (id, blob) = row?;
            if let Some(blob) = blob {
                if blob.len().is_multiple_of(4) && !blob.is_empty() {
                    map.insert(id, super::search::blob_to_embedding(&blob));
                }
            }
        }
        Ok(map)
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

    /// Every chunk for a file, ordered by `seq`, capped at `limit` (0 = no cap).
    /// Embeddings are deliberately left `None`: this backs the MCP
    /// `get_chunk_context` tool, which displays a file's indexed text/headings
    /// (and neighbors of a search hit), not vectors — so the BLOB stays off the wire.
    pub fn chunks_for_path(&self, entry_path: &str, limit: usize) -> Result<Vec<ChunkRecord>> {
        let lim: i64 = if limit == 0 { -1 } else { limit as i64 };
        let mut stmt = self.conn.prepare(
            "SELECT seq, heading, text, language FROM chunks
              WHERE entry_path = ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![entry_path, lim], |r| {
            Ok(ChunkRecord {
                entry_path: entry_path.to_owned(),
                seq: r.get::<_, i64>(0)? as usize,
                heading: r.get::<_, String>(1)?,
                text: r.get::<_, String>(2)?,
                language: r.get::<_, Option<String>>(3)?,
                embedding: None,
                embed_model: None,
                content_hash: None,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Code chunks (those carrying a `language` tag) under `prefix` — the file itself, or any
    /// descendant when `prefix` is a directory — ordered by path then seq. Powers the v0.31
    /// signatures (code-skeleton) export. `limit == 0` means no limit.
    pub fn code_chunks_under(&self, prefix: &str, limit: usize) -> Result<Vec<ChunkRecord>> {
        let lim: i64 = if limit == 0 { -1 } else { limit as i64 };
        let like = super::search::like_prefix(&format!("{}/", prefix.trim_end_matches('/')));
        let mut stmt = self.conn.prepare(
            "SELECT entry_path, seq, heading, text, language FROM chunks
              WHERE (entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\')
                AND language IS NOT NULL
              ORDER BY entry_path, seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![prefix, like, lim], |r| {
            Ok(ChunkRecord {
                entry_path: r.get::<_, String>(0)?,
                seq: r.get::<_, i64>(1)? as usize,
                heading: r.get::<_, String>(2)?,
                text: r.get::<_, String>(3)?,
                language: r.get::<_, Option<String>>(4)?,
                embedding: None,
                embed_model: None,
                content_hash: None,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod chunk_cache_tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn cached_embeddings_returns_stored_vector_for_matching_hash() {
        let mut store = Store::open_in_memory().unwrap();

        // Insert a chunk with a known hash and embedding.
        let vec = vec![0.1_f32, 0.2, 0.3];
        store
            .upsert_chunks(&[ChunkRecord {
                entry_path: "/test/a.rs".to_owned(),
                seq: 0,
                heading: String::new(),
                text: "hello world".to_owned(),
                language: None,
                embedding: Some(vec.clone()),
                embed_model: Some("nomic".to_owned()),
                content_hash: Some("abc123".to_owned()),
            }])
            .unwrap();

        let cache = store.cached_embeddings_by_hash("/test/a.rs").unwrap();
        assert!(
            cache.contains_key("abc123"),
            "expected hash key 'abc123' in cache"
        );
        let cached_vec = &cache["abc123"];
        // Round-trip through f32 LE bytes so we use approx equality.
        for (a, b) in vec.iter().zip(cached_vec.iter()) {
            assert!((a - b).abs() < 1e-6, "embedding mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn cached_embeddings_excludes_null_embedding_rows() {
        let mut store = Store::open_in_memory().unwrap();

        // A chunk with a hash but NULL embedding (prior embed failure).
        store
            .upsert_chunks(&[ChunkRecord {
                entry_path: "/test/b.rs".to_owned(),
                seq: 0,
                heading: String::new(),
                text: "content".to_owned(),
                language: None,
                embedding: None, // no vector stored
                embed_model: None,
                content_hash: Some("deadbeef".to_owned()),
            }])
            .unwrap();

        let cache = store.cached_embeddings_by_hash("/test/b.rs").unwrap();
        assert!(
            cache.is_empty(),
            "a chunk with NULL embedding must not appear in the cache"
        );
    }

    #[test]
    fn cached_embeddings_excludes_null_hash_rows() {
        let mut store = Store::open_in_memory().unwrap();

        // A legacy chunk with no hash (pre-v0.42 row).
        store
            .upsert_chunks(&[ChunkRecord {
                entry_path: "/test/c.rs".to_owned(),
                seq: 0,
                heading: String::new(),
                text: "old content".to_owned(),
                language: None,
                embedding: Some(vec![0.5, 0.6]),
                embed_model: Some("nomic".to_owned()),
                content_hash: None, // legacy row
            }])
            .unwrap();

        let cache = store.cached_embeddings_by_hash("/test/c.rs").unwrap();
        assert!(
            cache.is_empty(),
            "a chunk with NULL content_hash must not appear in the cache"
        );
    }
}
