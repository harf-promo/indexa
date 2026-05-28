use crate::config::HybridMode;
use crate::walker::{Entry, EntryKind};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

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
        let store = Self { conn, db_path: path.to_path_buf() };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn, db_path: PathBuf::from(":memory:") };
        store.init_schema()?;
        Ok(store)
    }

    /// Path to the on-disk database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            -- Surface-scan entries (paths, sizes, surface hints)
            CREATE TABLE IF NOT EXISTS entries (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                parent_path TEXT,
                kind        TEXT NOT NULL CHECK(kind IN ('file','dir')),
                size        INTEGER NOT NULL DEFAULT 0,
                modified_s  INTEGER,
                hint_label  TEXT,
                hint_cat    TEXT,
                deep_policy TEXT,
                indexed_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_path);
            CREATE INDEX IF NOT EXISTS idx_entries_kind   ON entries(kind);
            CREATE INDEX IF NOT EXISTS idx_entries_cat    ON entries(hint_cat);

            -- Deep-scan chunks (text + embeddings)
            CREATE TABLE IF NOT EXISTS chunks (
                id          INTEGER PRIMARY KEY,
                entry_path  TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                heading     TEXT NOT NULL DEFAULT '',
                text        TEXT NOT NULL,
                language    TEXT,
                embedding   BLOB,              -- IEEE-754 f32 little-endian bytes
                embed_model TEXT,
                indexed_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE (entry_path, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(entry_path);

            -- FTS5 full-text search over chunk text (standalone, populated manually)
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                text,
                heading,
                entry_path,
                chunk_id
            );
            ",
        )?;
        Ok(())
    }

    // ── Surface-scan writes ───────────────────────────────────────────────────

    /// Insert or replace a batch of walker entries.
    pub fn upsert_entries(&mut self, entries: &[Entry]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO entries
                 (path, parent_path, kind, size, modified_s, hint_label, hint_cat, deep_policy)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;

            for e in entries {
                let path_str = e.path.to_string_lossy();
                let parent_str = e.path.parent().map(|p| p.to_string_lossy().into_owned());
                let kind = match e.kind {
                    EntryKind::File => "file",
                    EntryKind::Dir => "dir",
                };
                let modified = e
                    .modified
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let (label, cat, policy) = e
                    .hint
                    .as_ref()
                    .map(|h| {
                        let p = format!("{:?}", h.deep_scan);
                        (Some(h.label), Some(h.category), Some(p))
                    })
                    .unwrap_or((None, None, None));

                stmt.execute(params![
                    path_str.as_ref(),
                    parent_str,
                    kind,
                    e.size as i64,
                    modified,
                    label,
                    cat,
                    policy,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Deep-scan writes ──────────────────────────────────────────────────────

    /// Insert or replace a batch of chunks (text + optional embedding).
    /// The FTS5 index is kept in sync via triggers / manual insert.
    pub fn upsert_chunks(&mut self, chunks: &[ChunkRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO chunks
                 (entry_path, seq, heading, text, language, embedding, embed_model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut fts_del =
                tx.prepare_cached("DELETE FROM chunks_fts WHERE chunk_id = CAST(?1 AS TEXT)")?;
            let mut fts_ins = tx.prepare_cached(
                "INSERT INTO chunks_fts(text, heading, entry_path, chunk_id)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;

            for c in chunks {
                let embedding_blob = c.embedding.as_ref().map(|v| {
                    // Store f32 vec as little-endian bytes
                    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
                    bytes
                });

                stmt.execute(params![
                    c.entry_path,
                    c.seq as i64,
                    c.heading,
                    c.text,
                    c.language,
                    embedding_blob,
                    c.embed_model,
                ])?;

                // Get the rowid just inserted / replaced
                let rowid = tx.last_insert_rowid();

                // Keep FTS5 in sync (delete any prior FTS entry, re-insert)
                fts_del.execute(params![rowid.to_string()])?;
                fts_ins.execute(params![c.text, c.heading, c.entry_path, rowid.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    /// Count of all indexed entries.
    pub fn entry_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        Ok(n as u64)
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

    /// Remove a single entry (and its chunks) from the index by exact path.
    pub fn delete_entry(&mut self, path: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM chunks_fts WHERE entry_path = ?1", params![path])?;
        tx.execute("DELETE FROM chunks WHERE entry_path = ?1", params![path])?;
        let n = tx.execute("DELETE FROM entries WHERE path = ?1", params![path])?;
        tx.commit()?;
        Ok(n)
    }

    /// Remove all entries whose path starts with `prefix` (e.g. a whole directory subtree).
    /// Returns the number of `entries` rows deleted.
    pub fn delete_subtree(&mut self, prefix: &str) -> Result<usize> {
        let pattern = format!("{prefix}%");
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chunks_fts WHERE entry_path LIKE ?1",
            params![pattern],
        )?;
        tx.execute(
            "DELETE FROM chunks WHERE entry_path LIKE ?1",
            params![pattern],
        )?;
        let n = tx.execute(
            "DELETE FROM entries WHERE path LIKE ?1 OR parent_path LIKE ?1",
            params![pattern],
        )?;
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
        let ts: Option<i64> = self
            .conn
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

    /// Summary of top-level regions: (category, entry_count, total_size_bytes).
    pub fn region_summary(&self) -> Result<Vec<RegionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(hint_cat, 'unknown') AS cat,
                    COUNT(*) AS cnt,
                    SUM(size) AS total_size
             FROM entries
             GROUP BY cat
             ORDER BY total_size DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RegionSummary {
                category: r.get(0)?,
                entry_count: r.get::<_, i64>(1)? as u64,
                total_size: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Hybrid search: FTS5 BM25 + embedding cosine similarity, fused with RRF.
    ///
    /// - `mode` selects which retrievers run (Rrf = both, Sparse = FTS only, Dense = cosine only)
    /// - `scope` optionally filters results to paths starting with the given prefix
    /// - `rrf_k` is the RRF rank constant (60 is the industry default)
    pub fn hybrid_search(
        &self,
        query_text: &str,
        query_embedding: Option<&[f32]>,
        mode: &HybridMode,
        scope: Option<&str>,
        limit: usize,
        rrf_k: f32,
    ) -> Result<Vec<SearchHit>> {
        let rrf_k = rrf_k as f64;

        // ── FTS5 keyword retrieval ────────────────────────────────────────────
        let fts_candidates: Vec<(i64, String)> = match mode {
            HybridMode::Dense => Vec::new(),
            HybridMode::Weighted => anyhow::bail!("weighted mode not yet implemented; use rrf"),
            _ => {
                let fts_query = fts5_quote(query_text);
                let (sql, scope_param) = if let Some(s) = scope {
                    (
                        "SELECT CAST(chunk_id AS INTEGER), entry_path, bm25(chunks_fts) AS score
                         FROM chunks_fts
                         WHERE chunks_fts MATCH ?1 AND entry_path LIKE ?2
                         ORDER BY score LIMIT 100"
                            .to_string(),
                        Some(format!("{s}%")),
                    )
                } else {
                    (
                        "SELECT CAST(chunk_id AS INTEGER), entry_path, bm25(chunks_fts) AS score
                         FROM chunks_fts
                         WHERE chunks_fts MATCH ?1
                         ORDER BY score LIMIT 100"
                            .to_string(),
                        None,
                    )
                };
                let mut stmt = self.conn.prepare(&sql)?;
                let rows: Vec<(i64, String)> = if let Some(ref sp) = scope_param {
                    stmt.query_map(params![fts_query, sp], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                } else {
                    stmt.query_map(params![fts_query], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                };
                rows
            }
        };

        // ── Dense embedding retrieval ────────────────────────────────────────
        let dense_candidates: Vec<(i64, String)> = match mode {
            HybridMode::Sparse => Vec::new(),
            HybridMode::Weighted => Vec::new(), // bailed above
            _ => {
                if let Some(qvec) = query_embedding {
                    self.cosine_search(qvec, 100, scope)?
                } else {
                    Vec::new()
                }
            }
        };

        // ── RRF fusion ────────────────────────────────────────────────────────
        use std::collections::HashMap;
        let mut scores: HashMap<i64, f64> = HashMap::new();
        let mut id_to_path: HashMap<i64, String> = HashMap::new();

        for (rank, (id, path)) in fts_candidates.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (rrf_k + rank as f64 + 1.0);
            id_to_path.entry(*id).or_insert_with(|| path.clone());
        }
        for (rank, (id, path)) in dense_candidates.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (rrf_k + rank as f64 + 1.0);
            id_to_path.entry(*id).or_insert_with(|| path.clone());
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        // ── Fetch chunk details for top results ──────────────────────────────
        let mut hits = Vec::with_capacity(ranked.len());
        for (id, rrf_score) in ranked {
            let mut stmt = self.conn.prepare_cached(
                "SELECT entry_path, seq, heading, text FROM chunks WHERE id = ?1",
            )?;
            if let Ok(hit) = stmt.query_row(params![id], |r| {
                Ok(SearchHit {
                    chunk_id: id,
                    entry_path: r.get(0)?,
                    seq: r.get::<_, i64>(1)? as usize,
                    heading: r.get(2)?,
                    text: r.get(3)?,
                    rrf_score,
                })
            }) {
                hits.push(hit);
            }
        }
        Ok(hits)
    }

    /// Brute-force cosine similarity over all stored embeddings.
    /// Returns (chunk_id, entry_path) sorted by descending similarity.
    fn cosine_search(
        &self,
        query: &[f32],
        limit: usize,
        scope: Option<&str>,
    ) -> Result<Vec<(i64, String)>> {
        let sql = if scope.is_some() {
            "SELECT id, entry_path, embedding FROM chunks WHERE embedding IS NOT NULL AND entry_path LIKE ?1"
        } else {
            "SELECT id, entry_path, embedding FROM chunks WHERE embedding IS NOT NULL"
        };
        let mut stmt = self.conn.prepare(sql)?;

        let mut scored: Vec<(i64, String, f32)> = Vec::new();
        let scope_pattern = scope.map(|s| format!("{s}%"));
        let mut rows = if let Some(ref p) = scope_pattern {
            stmt.query(params![p])?
        } else {
            stmt.query([])?
        };

        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let path: String = row.get(1)?;
            let blob: Vec<u8> = row.get(2)?;

            // Deserialize f32 little-endian bytes
            if !blob.len().is_multiple_of(4) {
                continue;
            }
            let vec: Vec<f32> = blob
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();

            if vec.len() != query.len() {
                continue;
            }

            let sim = cosine_similarity(query, &vec);
            scored.push((id, path, sim));
        }

        scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(id, path, _)| (id, path)).collect())
    }
}

/// Wrap a user query in FTS5 double-quote syntax so arbitrary natural-language
/// text (with punctuation, `?`, parentheses, etc.) is treated as a phrase search
/// rather than triggering FTS5 query-syntax errors.
fn fts5_quote(s: &str) -> String {
    // Escape any embedded double-quotes by doubling them.
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub entry_path: String,
    pub seq: usize,
    pub heading: String,
    pub text: String,
    pub language: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub embed_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub entry_path: String,
    pub seq: usize,
    pub heading: String,
    pub text: String,
    pub rrf_score: f64,
}

#[derive(Debug)]
pub struct RegionSummary {
    pub category: String,
    pub entry_count: u64,
    pub total_size: u64,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::{Entry, EntryKind};
    use std::path::PathBuf;

    fn dummy_entry(path: &str, kind: EntryKind, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size,
            modified: None,
            hint: None,
        }
    }

    fn dummy_chunk(path: &str, seq: usize, text: &str) -> ChunkRecord {
        ChunkRecord {
            entry_path: path.to_owned(),
            seq,
            heading: String::new(),
            text: text.to_owned(),
            language: None,
            embedding: None,
            embed_model: None,
        }
    }

    #[test]
    fn open_in_memory_and_upsert() {
        let mut store = Store::open_in_memory().unwrap();
        let entries = vec![
            dummy_entry("/home/user/file.txt", EntryKind::File, 1024),
            dummy_entry("/home/user/docs", EntryKind::Dir, 0),
        ];
        store.upsert_entries(&entries).unwrap();
        assert_eq!(store.entry_count().unwrap(), 2);
    }

    #[test]
    fn upsert_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let e = vec![dummy_entry("/tmp/a.txt", EntryKind::File, 10)];
        store.upsert_entries(&e).unwrap();
        store.upsert_entries(&e).unwrap();
        assert_eq!(store.entry_count().unwrap(), 1);
    }

    #[test]
    fn region_summary_groups_by_category() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[dummy_entry("/a.txt", EntryKind::File, 100)])
            .unwrap();
        let summary = store.region_summary().unwrap();
        assert!(!summary.is_empty());
    }

    #[test]
    fn chunks_indexed_and_fts_searchable() {
        let mut store = Store::open_in_memory().unwrap();
        let chunks = vec![
            dummy_chunk("/doc.md", 0, "the quick brown fox jumps over the lazy dog"),
            dummy_chunk(
                "/doc.md",
                1,
                "machine learning fundamentals and neural networks",
            ),
        ];
        store.upsert_chunks(&chunks).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);

        let hits = store
            .hybrid_search("machine learning", None, &HybridMode::Rrf, None, 10, 60.0)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].text.contains("machine learning"));
    }

    #[test]
    fn hybrid_search_with_embedding() {
        let mut store = Store::open_in_memory().unwrap();
        // Simple 3-dim embeddings for test
        let mut c1 = dummy_chunk("/a.md", 0, "cats and kittens");
        c1.embedding = Some(vec![1.0, 0.0, 0.0]);
        let mut c2 = dummy_chunk("/b.md", 0, "dogs and puppies");
        c2.embedding = Some(vec![0.0, 1.0, 0.0]);
        store.upsert_chunks(&[c1, c2]).unwrap();

        let query_vec = vec![1.0_f32, 0.0, 0.0];
        let hits = store
            .hybrid_search("cats", Some(&query_vec), &HybridMode::Rrf, None, 10, 60.0)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].entry_path.contains("/a.md"));
    }

    #[test]
    fn chunk_upsert_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let c = dummy_chunk("/x.txt", 0, "hello world");
        store.upsert_chunks(&[c.clone()]).unwrap();
        store.upsert_chunks(&[c]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
    }
}
