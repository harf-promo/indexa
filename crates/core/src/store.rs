use crate::config::HybridMode;
use crate::walker::{Entry, EntryKind};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
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

            -- Hierarchical summaries (one row per file or directory)
            CREATE TABLE IF NOT EXISTS summaries (
                path          TEXT PRIMARY KEY,
                kind          TEXT NOT NULL CHECK(kind IN ('file','dir')),
                parent_path   TEXT,
                depth         INTEGER NOT NULL DEFAULT 0,
                summary       TEXT NOT NULL,
                embedding     BLOB,
                child_count   INTEGER NOT NULL DEFAULT 0,
                byte_size     INTEGER NOT NULL DEFAULT 0,
                model         TEXT NOT NULL DEFAULT '',
                source_hash   TEXT NOT NULL DEFAULT '',
                generated_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_summaries_parent ON summaries(parent_path);
            CREATE INDEX IF NOT EXISTS idx_summaries_depth  ON summaries(depth);
            CREATE INDEX IF NOT EXISTS idx_summaries_kind   ON summaries(kind);

            -- Background summarization queue
            CREATE TABLE IF NOT EXISTS summary_queue (
                path        TEXT PRIMARY KEY,
                kind        TEXT NOT NULL CHECK(kind IN ('file','dir')),
                depth       INTEGER NOT NULL DEFAULT 0,
                state       TEXT NOT NULL DEFAULT 'pending'
                                 CHECK(state IN ('pending','in_flight','done','failed')),
                attempts    INTEGER NOT NULL DEFAULT 0,
                enqueued_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                error       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_summary_queue_state ON summary_queue(state);
            ",
        )?;
        Ok(())
    }

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
        tx.execute(
            "DELETE FROM chunks_fts WHERE entry_path = ?1",
            params![path],
        )?;
        tx.execute("DELETE FROM chunks WHERE entry_path = ?1", params![path])?;
        let n = tx.execute("DELETE FROM entries WHERE path = ?1", params![path])?;
        tx.commit()?;
        Ok(n)
    }

    /// Reconcile entries under `root_prefix` against the live set returned by a fresh walk.
    /// Deletes rows (plus their chunks and summaries) for paths no longer on disk.
    /// Returns the number of entry rows removed.
    pub fn reconcile_entries(
        &mut self,
        root_prefix: &str,
        live_paths: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let pattern = like_prefix(root_prefix);
        let indexed_paths: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path FROM entries WHERE path LIKE ?1 ESCAPE '\\'")?;
            let rows = stmt.query_map(params![pattern], |r| r.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?
        };

        let ghosts: Vec<String> = indexed_paths
            .into_iter()
            .filter(|p| !live_paths.contains(p))
            .collect();

        if ghosts.is_empty() {
            return Ok(0);
        }

        let tx = self.conn.transaction()?;
        let mut removed = 0usize;
        for path in &ghosts {
            tx.execute(
                "DELETE FROM chunks_fts WHERE entry_path = ?1",
                params![path],
            )?;
            tx.execute("DELETE FROM chunks WHERE entry_path = ?1", params![path])?;
            tx.execute("DELETE FROM summaries WHERE path = ?1", params![path])?;
            removed += tx.execute("DELETE FROM entries WHERE path = ?1", params![path])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Remove all entries whose path starts with `prefix` (e.g. a whole directory subtree).
    /// Returns the number of `entries` rows deleted.
    pub fn delete_subtree(&mut self, prefix: &str) -> Result<usize> {
        let pattern = like_prefix(prefix);
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chunks_fts WHERE entry_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        tx.execute(
            "DELETE FROM chunks WHERE entry_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        let n = tx.execute(
            "DELETE FROM entries WHERE path LIKE ?1 ESCAPE '\\' OR parent_path LIKE ?1 ESCAPE '\\'",
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

    /// Top-N file extensions (or bare names) in the index that have no category assigned.
    /// Returns (extension_or_name, count) pairs sorted by count descending.
    pub fn unknown_extensions(&self, limit: usize) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT
               CASE
                 WHEN instr(path, '.') > 0
                      AND instr(replace(path, rtrim(path, replace(path, '/', '')), ''), '.') > 0
                 THEN lower(substr(path, length(path) - length(path) + instr(path, '.')))
                 ELSE '(no extension)'
               END AS ext,
               COUNT(*) AS n
             FROM entries
             WHERE (hint_cat IS NULL OR hint_cat = 'unknown') AND kind = 'file'
             GROUP BY ext
             ORDER BY n DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
                         WHERE chunks_fts MATCH ?1 AND entry_path LIKE ?2 ESCAPE '\\'
                         ORDER BY score LIMIT 100"
                            .to_string(),
                        Some(like_prefix(s)),
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

    // ── Misc helpers ─────────────────────────────────────────────────────────

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

    /// Expose the raw rusqlite connection for use in helper modules that need
    /// to run queries not covered by the public API (e.g. summarize.rs).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// All (path, kind) entries under `root` that are not yet in summary_queue
    /// and whose deep_policy is not 'Skip'.
    pub fn entries_for_summarization(&self, root: &str) -> Result<Vec<(String, String)>> {
        let pattern = like_prefix(root);
        let mut stmt = self.conn.prepare(
            "SELECT path, kind FROM entries
             WHERE (path LIKE ?1 ESCAPE '\\' OR parent_path LIKE ?1 ESCAPE '\\')
               AND path NOT IN (SELECT path FROM summary_queue)
               AND (deep_policy IS NULL OR deep_policy != 'Skip')",
        )?;
        let rows = stmt.query_map(params![pattern], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Delete chunks for every file whose path is under `prefix`.
    pub fn delete_chunks_for_subtree(&mut self, prefix: &str) -> Result<usize> {
        let pattern = like_prefix(prefix);
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chunks_fts WHERE entry_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        let n = tx.execute(
            "DELETE FROM chunks WHERE entry_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        tx.commit()?;
        Ok(n)
    }

    // ── Summary writes ────────────────────────────────────────────────────────

    /// Insert or replace a summary row.
    pub fn upsert_summary(&mut self, record: &SummaryRecord) -> Result<()> {
        let embedding_blob = record
            .embedding
            .as_ref()
            .map(|v| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>());
        self.conn.execute(
            "INSERT OR REPLACE INTO summaries
             (path, kind, parent_path, depth, summary, embedding,
              child_count, byte_size, model, source_hash, generated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                record.path,
                record.kind,
                record.parent_path,
                record.depth,
                record.summary,
                embedding_blob,
                record.child_count,
                record.byte_size,
                record.model,
                record.source_hash,
                record.generated_at,
            ],
        )?;
        Ok(())
    }

    /// Look up a single summary row by exact path.
    pub fn summary_by_path(&self, path: &str) -> Result<Option<SummaryRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, kind, parent_path, depth, summary, embedding,
                    child_count, byte_size, model, source_hash, generated_at
             FROM summaries WHERE path = ?1",
        )?;
        let row = stmt.query_row(params![path], |r| {
            let blob: Option<Vec<u8>> = r.get(5)?;
            let embedding = blob.map(|b| {
                b.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            });
            Ok(SummaryRecord {
                path: r.get(0)?,
                kind: r.get(1)?,
                parent_path: r.get(2)?,
                depth: r.get(3)?,
                summary: r.get(4)?,
                embedding,
                child_count: r.get(6)?,
                byte_size: r.get(7)?,
                model: r.get(8)?,
                source_hash: r.get(9)?,
                generated_at: r.get(10)?,
            })
        });
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All summary rows whose parent_path == given path (direct children).
    pub fn children_summaries(&self, parent_path: &str) -> Result<Vec<SummaryRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, kind, parent_path, depth, summary, embedding,
                    child_count, byte_size, model, source_hash, generated_at
             FROM summaries WHERE parent_path = ?1 ORDER BY kind DESC, path",
        )?;
        let rows = stmt.query_map(params![parent_path], |r| {
            let blob: Option<Vec<u8>> = r.get(5)?;
            let embedding = blob.map(|b| {
                b.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            });
            Ok(SummaryRecord {
                path: r.get(0)?,
                kind: r.get(1)?,
                parent_path: r.get(2)?,
                depth: r.get(3)?,
                summary: r.get(4)?,
                embedding,
                child_count: r.get(6)?,
                byte_size: r.get(7)?,
                model: r.get(8)?,
                source_hash: r.get(9)?,
                generated_at: r.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Ancestor chain from path up to root (breadcrumb), ordered shallow→deep.
    pub fn ancestor_summaries(&self, path: &str) -> Result<Vec<SummaryRecord>> {
        let mut crumbs: Vec<SummaryRecord> = Vec::new();
        let mut current = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned());
        while let Some(p) = current {
            if p.is_empty() || p == "/" {
                break;
            }
            if let Some(rec) = self.summary_by_path(&p)? {
                current = rec.parent_path.clone();
                crumbs.push(rec);
            } else {
                current = std::path::Path::new(&p)
                    .parent()
                    .map(|pp| pp.to_string_lossy().into_owned());
            }
        }
        crumbs.reverse();
        Ok(crumbs)
    }

    /// One level of the tree: entries under `parent_path` with their summary states.
    pub fn tree_level(&self, parent_path: &str) -> Result<Vec<TreeNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind, e.size,
                    sq.state AS summary_state
             FROM entries e
             LEFT JOIN summary_queue sq ON sq.path = e.path
             WHERE e.parent_path = ?1
             ORDER BY e.kind DESC, e.path",
        )?;
        let rows = stmt.query_map(params![parent_path], |r| {
            let full_path: String = r.get(0)?;
            let name = std::path::Path::new(&full_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| full_path.clone());
            Ok(TreeNode {
                path: full_path,
                name,
                kind: r.get(1)?,
                byte_size: r.get::<_, i64>(2)?,
                child_count: 0,
                summary_state: r.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Count of summary rows.
    pub fn summary_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Brute-force cosine search over summary embeddings.
    /// Returns (path, similarity) sorted descending, with depth boosting applied.
    pub fn summary_cosine_search(
        &self,
        query: &[f32],
        limit: usize,
        depth_alpha: f32,
    ) -> Result<Vec<(String, f32)>> {
        let max_depth: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(depth), 0) FROM summaries", [], |r| {
                    r.get(0)
                })?;

        let mut stmt = self
            .conn
            .prepare("SELECT path, depth, embedding FROM summaries WHERE embedding IS NOT NULL")?;
        let mut scored: Vec<(String, f32)> = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let depth: i64 = row.get(1)?;
            let blob: Vec<u8> = row.get(2)?;
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
            let boost = 1.0 + depth_alpha * (max_depth - depth) as f32;
            scored.push((path, sim * boost));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    // ── Summary queue ────────────────────────────────────────────────────────

    /// Enqueue (path, kind, depth) items; ignores duplicates.
    pub fn enqueue_summary_items(&mut self, items: &[(String, String, i64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO summary_queue (path, kind, depth, state)
                 VALUES (?1, ?2, ?3, 'pending')",
            )?;
            for (path, kind, depth) in items {
                stmt.execute(params![path, kind, depth])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Take one pending item — deepest first (files before their parent dirs).
    pub fn next_queue_item(&mut self) -> Result<Option<QueueItem>> {
        let item = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT path, kind, depth FROM summary_queue
                 WHERE state = 'pending'
                 ORDER BY depth DESC LIMIT 1",
            )?;
            stmt.query_row([], |r| {
                Ok(QueueItem {
                    path: r.get(0)?,
                    kind: r.get(1)?,
                    depth: r.get(2)?,
                })
            })
            .optional()?
        };
        if let Some(ref it) = item {
            self.conn.execute(
                "UPDATE summary_queue
                 SET state='in_flight', attempts=attempts+1, updated_at=unixepoch()
                 WHERE path=?1",
                params![it.path],
            )?;
        }
        Ok(item)
    }

    /// Mark a queue item's state (e.g. "done" or "failed").
    pub fn mark_queue_state(&mut self, path: &str, state: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE summary_queue SET state=?1, error=?2, updated_at=unixepoch() WHERE path=?3",
            params![state, error, path],
        )?;
        Ok(())
    }

    /// Queue statistics for status display.
    pub fn queue_stats(&self) -> Result<QueueStats> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM summary_queue GROUP BY state")?;
        let mut stats = QueueStats::default();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let state: String = row.get(0)?;
            let n: i64 = row.get(1)?;
            match state.as_str() {
                "pending" => stats.pending = n,
                "in_flight" => stats.in_flight = n,
                "done" => stats.done = n,
                "failed" => stats.failed = n,
                _ => {}
            }
        }
        Ok(stats)
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
            "SELECT id, entry_path, embedding FROM chunks WHERE embedding IS NOT NULL AND entry_path LIKE ?1 ESCAPE '\\'"
        } else {
            "SELECT id, entry_path, embedding FROM chunks WHERE embedding IS NOT NULL"
        };
        let mut stmt = self.conn.prepare(sql)?;

        let mut scored: Vec<(i64, String, f32)> = Vec::new();
        let scope_pattern = scope.map(like_prefix);
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

/// Escape `%` and `_` wildcards in a path prefix before appending `%` for LIKE matching.
/// Must be used with `LIKE ?n ESCAPE '\'` in the SQL clause.
fn like_prefix(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
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

#[derive(Debug, Clone)]
pub struct SummaryRecord {
    pub path: String,
    pub kind: String,
    pub parent_path: Option<String>,
    pub depth: i64,
    pub summary: String,
    pub embedding: Option<Vec<f32>>,
    pub child_count: i64,
    pub byte_size: i64,
    pub model: String,
    pub source_hash: String,
    pub generated_at: i64,
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub child_count: i64,
    pub byte_size: i64,
    pub summary_state: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    pub pending: i64,
    pub in_flight: i64,
    pub done: i64,
    pub failed: i64,
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub path: String,
    pub kind: String,
    pub depth: i64,
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
        store.upsert_chunks(std::slice::from_ref(&c)).unwrap();
        store.upsert_chunks(&[c]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
    }

    #[test]
    fn delete_entry_removes_entry_and_chunks() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[dummy_entry("/notes.txt", EntryKind::File, 100)])
            .unwrap();
        store
            .upsert_chunks(&[dummy_chunk("/notes.txt", 0, "hello world")])
            .unwrap();
        assert_eq!(store.entry_count().unwrap(), 1);
        assert_eq!(store.chunk_count().unwrap(), 1);

        let deleted = store.delete_entry("/notes.txt").unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(store.entry_count().unwrap(), 0);
        assert_eq!(store.chunk_count().unwrap(), 0);
    }

    #[test]
    fn delete_subtree_removes_all_under_prefix() {
        let mut store = Store::open_in_memory().unwrap();
        let entries = vec![
            dummy_entry("/docs/a.txt", EntryKind::File, 10),
            dummy_entry("/docs/b.txt", EntryKind::File, 10),
            dummy_entry("/other/c.txt", EntryKind::File, 10),
        ];
        store.upsert_entries(&entries).unwrap();
        assert_eq!(store.entry_count().unwrap(), 3);

        let deleted = store.delete_subtree("/docs/").unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(store.entry_count().unwrap(), 1);
    }

    #[test]
    fn delete_subtree_does_not_over_delete_similar_prefix() {
        // "/foo" delete must NOT remove "/foobar/file.txt"
        let mut store = Store::open_in_memory().unwrap();
        let entries = vec![
            dummy_entry("/foo/a.txt", EntryKind::File, 10),
            dummy_entry("/foobar/b.txt", EntryKind::File, 10),
        ];
        store.upsert_entries(&entries).unwrap();

        store.delete_subtree("/foo/").unwrap();

        assert_eq!(
            store.entry_count().unwrap(),
            1,
            "/foobar/b.txt should survive"
        );
    }

    #[test]
    fn hybrid_search_sparse_mode_returns_fts_results() {
        let mut store = Store::open_in_memory().unwrap();
        let chunks = vec![dummy_chunk("/doc.md", 0, "indexa sparse retrieval test")];
        store.upsert_chunks(&chunks).unwrap();

        let hits = store
            .hybrid_search("sparse", None, &HybridMode::Sparse, None, 5, 60.0)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].text.contains("sparse"));
    }

    #[test]
    fn hybrid_search_dense_mode_returns_vector_results() {
        let mut store = Store::open_in_memory().unwrap();
        let mut c = dummy_chunk("/vec.md", 0, "dense vector search");
        c.embedding = Some(vec![1.0, 0.0, 0.0]);
        store.upsert_chunks(&[c]).unwrap();

        let query_vec = vec![1.0_f32, 0.0, 0.0];
        let hits = store
            .hybrid_search("dense", Some(&query_vec), &HybridMode::Dense, None, 5, 60.0)
            .unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn hybrid_search_scope_filters_by_path_prefix() {
        let mut store = Store::open_in_memory().unwrap();
        let chunks = vec![
            dummy_chunk("/docs/tax/form.pdf", 0, "tax return income"),
            dummy_chunk("/photos/vacation.jpg", 0, "vacation photo hawaii"),
        ];
        store.upsert_chunks(&chunks).unwrap();

        let hits = store
            .hybrid_search(
                "vacation",
                None,
                &HybridMode::Sparse,
                Some("/docs/"),
                10,
                60.0,
            )
            .unwrap();
        assert!(
            hits.is_empty(),
            "scope /docs/ should exclude /photos/ results"
        );
    }

    #[test]
    fn fts5_quote_escapes_double_quotes() {
        let quoted = fts5_quote(r#"he said "hello""#);
        assert!(quoted.starts_with('"'));
        assert!(quoted.ends_with('"'));
        assert!(
            quoted.contains(r#""""#),
            "embedded quotes should be doubled: {quoted}"
        );
    }

    #[test]
    fn like_prefix_escapes_wildcards_in_path() {
        let p = like_prefix("/home/user/50%_done/");
        assert!(p.contains("\\%"), "% should be escaped: {p}");
        assert!(p.contains("\\_"), "_ should be escaped: {p}");
        assert!(
            p.ends_with('%'),
            "pattern should end with trailing wildcard: {p}"
        );
    }

    fn dummy_summary(path: &str, kind: &str, parent: Option<&str>, depth: i64) -> SummaryRecord {
        SummaryRecord {
            path: path.to_owned(),
            kind: kind.to_owned(),
            parent_path: parent.map(|s| s.to_owned()),
            depth,
            summary: format!("summary of {path}"),
            embedding: None,
            child_count: 0,
            byte_size: 100,
            model: "gemma2:2b".to_owned(),
            source_hash: String::new(),
            generated_at: 0,
        }
    }

    #[test]
    fn summaries_upsert_and_lookup() {
        let mut store = Store::open_in_memory().unwrap();
        let rec = dummy_summary("/docs/file.txt", "file", Some("/docs"), 2);
        store.upsert_summary(&rec).unwrap();
        assert_eq!(store.summary_count().unwrap(), 1);

        let got = store.summary_by_path("/docs/file.txt").unwrap().unwrap();
        assert_eq!(got.kind, "file");
        assert_eq!(got.summary, "summary of /docs/file.txt");
    }

    #[test]
    fn summaries_upsert_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let rec = dummy_summary("/a.txt", "file", Some("/"), 1);
        store.upsert_summary(&rec).unwrap();
        store.upsert_summary(&rec).unwrap();
        assert_eq!(store.summary_count().unwrap(), 1);
    }

    #[test]
    fn children_summaries_returns_direct_children() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&dummy_summary("/docs/a.txt", "file", Some("/docs"), 2))
            .unwrap();
        store
            .upsert_summary(&dummy_summary("/docs/b.txt", "file", Some("/docs"), 2))
            .unwrap();
        store
            .upsert_summary(&dummy_summary("/other/c.txt", "file", Some("/other"), 2))
            .unwrap();

        let children = store.children_summaries("/docs").unwrap();
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|c| c.parent_path.as_deref() == Some("/docs")));
    }

    #[test]
    fn summary_queue_enqueue_and_dequeue() {
        let mut store = Store::open_in_memory().unwrap();
        let items = vec![
            ("/docs/a.txt".to_owned(), "file".to_owned(), 2i64),
            ("/docs/b.txt".to_owned(), "file".to_owned(), 2i64),
        ];
        store.enqueue_summary_items(&items).unwrap();

        let stats = store.queue_stats().unwrap();
        assert_eq!(stats.pending, 2);

        let item = store.next_queue_item().unwrap().unwrap();
        assert_eq!(item.kind, "file");

        let stats2 = store.queue_stats().unwrap();
        assert_eq!(stats2.in_flight, 1);
        assert_eq!(stats2.pending, 1);

        store.mark_queue_state(&item.path, "done", None).unwrap();
        let stats3 = store.queue_stats().unwrap();
        assert_eq!(stats3.done, 1);
    }

    #[test]
    fn summary_cosine_search_returns_boosted_results() {
        let mut store = Store::open_in_memory().unwrap();
        let mut root = dummy_summary("/", "dir", None, 0);
        root.embedding = Some(vec![1.0, 0.0, 0.0]);
        let mut leaf = dummy_summary("/docs/file.txt", "file", Some("/docs"), 2);
        leaf.embedding = Some(vec![1.0, 0.0, 0.0]);
        store.upsert_summary(&root).unwrap();
        store.upsert_summary(&leaf).unwrap();

        let results = store
            .summary_cosine_search(&[1.0, 0.0, 0.0], 10, 0.15)
            .unwrap();
        assert!(!results.is_empty());
        // Root (depth=0) should score higher than leaf (depth=2) due to depth boost
        assert_eq!(results[0].0, "/");
    }
}
