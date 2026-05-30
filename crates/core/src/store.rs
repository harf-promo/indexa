use crate::config::HybridMode;
use crate::walker::{Entry, EntryKind};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use std::path::{Path, PathBuf};

// ── Private encoding/decoding helpers ─────────────────────────────────────────

/// Encode an `f32` vector as a little-endian byte blob (4 bytes per f32).
fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode a little-endian byte blob back into an `f32` vector.
///
/// Any trailing bytes that don't form a complete 4-byte chunk are silently
/// dropped (via `chunks_exact`), matching the historical behavior at the
/// summary call sites. Callers that need strict alignment validation should
/// check `b.len().is_multiple_of(4)` before calling.
fn blob_to_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Derive an L0 one-line abstract from a fuller (L1) summary: the first sentence,
/// truncated to ~120 chars on a char boundary. Used both when writing new summaries
/// and as a lazy fallback for rows stored before tiered summaries existed.
pub fn abstract_from(summary: &str) -> String {
    let trimmed = summary.trim();
    // First sentence: up to the first '. ', '! ', '? ', or newline.
    let end = trimmed
        .char_indices()
        .find(|(i, c)| {
            matches!(c, '.' | '!' | '?')
                && trimmed[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .map(|n| n.is_whitespace())
                    .unwrap_or(true)
        })
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(trimmed.len());
    let first = trimmed[..end].trim();
    // Cap length on a char boundary.
    const MAX: usize = 120;
    if first.len() <= MAX {
        return first.to_owned();
    }
    let mut cut = MAX;
    while cut > 0 && !first.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", first[..cut].trim_end())
}

// ── Private row mappers ───────────────────────────────────────────────────────

/// Map a row from the `summaries` table (in the canonical column order used by
/// `summary_by_path` and `children_summaries`) into a `SummaryRecord`.
/// Column order: path, kind, parent_path, depth, summary, summary_l0, embedding,
/// child_count, byte_size, model, source_hash, generated_at.
fn row_to_summary(r: &Row) -> rusqlite::Result<SummaryRecord> {
    let summary: String = r.get(4)?;
    // Lazily derive L0 for rows written before the summary_l0 column existed.
    let summary_l0: Option<String> = r
        .get::<_, Option<String>>(5)?
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(abstract_from(&summary)));
    let blob: Option<Vec<u8>> = r.get(6)?;
    Ok(SummaryRecord {
        path: r.get(0)?,
        kind: r.get(1)?,
        parent_path: r.get(2)?,
        depth: r.get(3)?,
        summary,
        summary_l0,
        embedding: blob.map(|b| blob_to_embedding(&b)),
        child_count: r.get(7)?,
        byte_size: r.get(8)?,
        model: r.get(9)?,
        source_hash: r.get(10)?,
        generated_at: r.get(11)?,
    })
}

/// Map a row from the `entries` + `summary_queue` join (used by `search_paths`
/// and `tree_level`) into a `TreeNode`.
/// Column order: path, kind, size, file_count, chunk_count, summary_state
fn row_to_tree_node(r: &Row) -> rusqlite::Result<TreeNode> {
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
        file_count: r.get::<_, i64>(3).unwrap_or(0),
        chunk_count: r.get::<_, i64>(4).unwrap_or(0),
        child_count: 0,
        summary_state: r.get(5)?,
    })
}

// ── Private transaction helpers ───────────────────────────────────────────────

/// Delete chunks (and their FTS5 entries) for every row whose `entry_path`
/// matches the LIKE pattern. Shared by `delete_subtree` and
/// `delete_chunks_for_subtree`.
fn delete_chunks_under_prefix(tx: &Transaction, pattern: &str) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM chunks_fts WHERE entry_path LIKE ?1 ESCAPE '\\'",
        params![pattern],
    )?;
    tx.execute(
        "DELETE FROM chunks WHERE entry_path LIKE ?1 ESCAPE '\\'",
        params![pattern],
    )
}

/// Delete all artifacts (chunks, FTS, summaries, entry) for one exact path.
/// Returns the number of `entries` rows removed (0 or 1). Used by
/// `reconcile_entries` to expunge a single ghost row.
fn delete_path_artifacts_exact(tx: &Transaction, path: &str) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM chunks_fts WHERE entry_path = ?1",
        params![path],
    )?;
    tx.execute("DELETE FROM chunks WHERE entry_path = ?1", params![path])?;
    tx.execute("DELETE FROM summaries WHERE path = ?1", params![path])?;
    tx.execute("DELETE FROM summary_queue WHERE path = ?1", params![path])?;
    tx.execute("DELETE FROM entries WHERE path = ?1", params![path])
}

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
            -- WAL allows one writer at a time across connections. The worker pool, the
            -- per-event watcher, and the web summarize path each open their own connection,
            -- so without a busy timeout a contended write fails immediately with SQLITE_BUSY.
            -- Block-and-retry for up to 5s instead.
            PRAGMA busy_timeout = 5000;

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
                summary_l0    TEXT,
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

        // Migration: add summaries.summary_l0 (L0 one-line abstract) to databases
        // created before tiered summaries existed. SQLite has no ADD COLUMN IF NOT
        // EXISTS, so we check table_info first and ignore if already present.
        let has_l0: bool = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('summaries') WHERE name = 'summary_l0'")?
            .exists([])?;
        if !has_l0 {
            self.conn
                .execute_batch("ALTER TABLE summaries ADD COLUMN summary_l0 TEXT;")?;
        }

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

    /// Remove a single entry (and its chunks, summary, and any queued summary work)
    /// from the index by exact path.
    pub fn delete_entry(&mut self, path: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM chunks_fts WHERE entry_path = ?1",
            params![path],
        )?;
        tx.execute("DELETE FROM chunks WHERE entry_path = ?1", params![path])?;
        // Keep the summary tables symmetric with chunks/entries: leaving these behind
        // orphans summary rows and (worse) leaves a stale summary_queue row that
        // `entries_for_summarization` filters on, permanently blocking re-summarization.
        tx.execute("DELETE FROM summaries WHERE path = ?1", params![path])?;
        tx.execute("DELETE FROM summary_queue WHERE path = ?1", params![path])?;
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
            removed += delete_path_artifacts_exact(&tx, path)?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Remove all entries whose path starts with `prefix` (e.g. a whole directory subtree),
    /// along with their chunks, summaries, and any queued summary work.
    /// Returns the number of `entries` rows deleted.
    pub fn delete_subtree(&mut self, prefix: &str) -> Result<usize> {
        let pattern = like_prefix(prefix);
        let tx = self.conn.transaction()?;
        delete_chunks_under_prefix(&tx, &pattern)?;
        // Summaries + queue must be cleared too (symmetry across all tables); an orphaned
        // summary_queue row would otherwise block re-summarization if the path is re-indexed.
        tx.execute(
            "DELETE FROM summaries WHERE path LIKE ?1 ESCAPE '\\' OR parent_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        tx.execute(
            "DELETE FROM summary_queue WHERE path LIKE ?1 ESCAPE '\\'",
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

    /// Top-N file extensions in the index that have no category assigned.
    /// Returns (`.ext` or `(no extension)`, count) pairs sorted by count descending,
    /// ties broken alphabetically.
    ///
    /// The extension is computed in Rust via `Path::extension` rather than in SQL: the
    /// previous pure-SQL expression used `length(path) - length(path)` (always 0), so it
    /// sliced from the *first* dot anywhere in the path (e.g. `/home/user.name/a.tar.gz`
    /// yielded `.name/a.tar.gz`) — a near-useless grouping.
    pub fn unknown_extensions(&self, limit: usize) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM entries
             WHERE (hint_cat IS NULL OR hint_cat = 'unknown') AND kind = 'file'",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;

        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for path in rows {
            let path = path?;
            let ext = Path::new(&path)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| "(no extension)".to_string());
            *counts.entry(ext).or_insert(0) += 1;
        }

        let mut sorted: Vec<(String, u64)> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        sorted.truncate(limit);
        Ok(sorted)
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
        let n = delete_chunks_under_prefix(&tx, &pattern)?;
        tx.commit()?;
        Ok(n)
    }

    // ── Summary writes ────────────────────────────────────────────────────────

    /// Insert or replace a summary row.
    pub fn upsert_summary(&mut self, record: &SummaryRecord) -> Result<()> {
        let embedding_blob = record.embedding.as_deref().map(embedding_to_blob);
        // Always persist an L0 abstract: use the provided one, else derive from L1.
        let l0 = record
            .summary_l0
            .clone()
            .unwrap_or_else(|| abstract_from(&record.summary));
        self.conn.execute(
            "INSERT OR REPLACE INTO summaries
             (path, kind, parent_path, depth, summary, summary_l0, embedding,
              child_count, byte_size, model, source_hash, generated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                record.path,
                record.kind,
                record.parent_path,
                record.depth,
                record.summary,
                l0,
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
            "SELECT path, kind, parent_path, depth, summary, summary_l0, embedding,
                    child_count, byte_size, model, source_hash, generated_at
             FROM summaries WHERE path = ?1",
        )?;
        stmt.query_row(params![path], row_to_summary)
            .optional()
            .map_err(Into::into)
    }

    /// All summary rows whose parent_path == given path (direct children).
    pub fn children_summaries(&self, parent_path: &str) -> Result<Vec<SummaryRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, kind, parent_path, depth, summary, summary_l0, embedding,
                    child_count, byte_size, model, source_hash, generated_at
             FROM summaries WHERE parent_path = ?1 ORDER BY kind DESC, path",
        )?;
        let rows = stmt.query_map(params![parent_path], row_to_summary)?;
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
    /// Return the implicit root paths — parent directories of indexed entries
    /// that are not themselves entries. These are the top-level nodes for the
    /// tree view when the user hasn't added an explicit root row.
    pub fn root_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT parent_path
               FROM entries e1
              WHERE parent_path != ''
                AND NOT EXISTS (
                    SELECT 1 FROM entries e2 WHERE e2.path = e1.parent_path
                )
              ORDER BY parent_path",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Search entries whose path contains `query` (case-insensitive LIKE).
    pub fn search_paths(&self, query: &str, limit: usize) -> Result<Vec<TreeNode>> {
        let pattern = format!("%{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind, e.size,
                    (SELECT COUNT(*) FROM entries c
                     WHERE c.parent_path = e.path AND c.kind = 'file') AS file_count,
                    (SELECT COUNT(*) FROM chunks
                     WHERE entry_path LIKE e.path || '/%') AS chunk_count,
                    sq.state AS summary_state
               FROM entries e
               LEFT JOIN summary_queue sq ON sq.path = e.path
              WHERE e.path LIKE ?1
              ORDER BY LENGTH(e.path) ASC, e.path ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], row_to_tree_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn tree_level(&self, parent_path: &str) -> Result<Vec<TreeNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind, e.size,
                    (SELECT COUNT(*) FROM entries c
                     WHERE c.parent_path = e.path AND c.kind = 'file') AS file_count,
                    (SELECT COUNT(*) FROM chunks
                     WHERE entry_path LIKE e.path || '/%') AS chunk_count,
                    sq.state AS summary_state
             FROM entries e
             LEFT JOIN summary_queue sq ON sq.path = e.path
             WHERE e.parent_path = ?1
             ORDER BY e.kind DESC, e.path",
        )?;
        let rows = stmt.query_map(params![parent_path], row_to_tree_node)?;
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
            let vec = blob_to_embedding(&blob);
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

    /// Optionally boost chunk-hit scores using parent-directory summary similarity.
    ///
    /// When `summary_weight == 0.0` this is a fast no-op. Otherwise it runs a
    /// summary cosine search and adds `summary_weight × summary_sim` to every
    /// hit whose `entry_path` falls under a matched summary directory.  Results
    /// are re-sorted after boosting.
    pub fn boost_with_summaries(
        &self,
        hits: &mut [SearchHit],
        query_vec: &[f32],
        summary_weight: f32,
        depth_alpha: f32,
    ) -> Result<()> {
        if summary_weight == 0.0 || hits.is_empty() {
            return Ok(());
        }
        let summaries = self.summary_cosine_search(query_vec, 20, depth_alpha)?;
        for hit in hits.iter_mut() {
            // Apply the score from the best matching summary (deepest parent wins).
            for (summary_path, sim) in &summaries {
                if hit.entry_path == *summary_path
                    || hit.entry_path.starts_with(&format!("{summary_path}/"))
                {
                    hit.rrf_score += (summary_weight as f64) * (*sim as f64);
                    break;
                }
            }
        }
        hits.sort_by(|a, b| {
            b.rrf_score
                .partial_cmp(&a.rrf_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(())
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

    /// Atomically claim one pending item — deepest first (files before their parent dirs).
    ///
    /// Uses a single `UPDATE ... WHERE path = (SELECT ... LIMIT 1) RETURNING` statement so the
    /// select-and-claim is one atomic write. The previous SELECT-then-separate-UPDATE let two
    /// connections (multiple workers + the web summarize path each open their own connection)
    /// read the same pending row before either flipped it, claiming and summarizing it twice.
    /// With WAL + `busy_timeout`, concurrent claims now serialize and each sees the prior claim.
    pub fn next_queue_item(&mut self) -> Result<Option<QueueItem>> {
        let item = self
            .conn
            .query_row(
                "UPDATE summary_queue
                 SET state='in_flight', attempts=attempts+1, updated_at=unixepoch()
                 WHERE path = (
                     SELECT path FROM summary_queue
                     WHERE state='pending'
                     ORDER BY depth DESC LIMIT 1
                 )
                 RETURNING path, kind, depth",
                [],
                |r| {
                    Ok(QueueItem {
                        path: r.get(0)?,
                        kind: r.get(1)?,
                        depth: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(item)
    }

    /// Reset items left `in_flight` by a previously crashed/killed run back to `pending`
    /// so they get retried; items whose `attempts` already reached `max_attempts` are marked
    /// `failed` instead (they keep crashing). Returns `(requeued, failed)`.
    ///
    /// Call this **once at process startup, before any worker begins claiming** — never while
    /// workers are draining, or it would reset an item another worker is actively processing.
    pub fn requeue_stale_in_flight(&mut self, max_attempts: u32) -> Result<(usize, usize)> {
        let tx = self.conn.transaction()?;
        let failed = tx.execute(
            "UPDATE summary_queue
             SET state='failed', error='exceeded max attempts after interruption',
                 updated_at=unixepoch()
             WHERE state='in_flight' AND attempts >= ?1",
            params![max_attempts],
        )?;
        let requeued = tx.execute(
            "UPDATE summary_queue
             SET state='pending', updated_at=unixepoch()
             WHERE state='in_flight'",
            [],
        )?;
        tx.commit()?;
        Ok((requeued, failed))
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

    /// Return up to `limit` items in the `failed` state, with their error messages.
    pub fn failed_queue_items(&self, limit: usize) -> Result<Vec<FailedQueueItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, error FROM summary_queue WHERE state = 'failed' ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(FailedQueueItem {
                path: r.get(0)?,
                error: r.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
            let vec = blob_to_embedding(&blob);

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
    /// L1 — the full 1–4 sentence summary.
    pub summary: String,
    /// L0 — a one-line abstract (first sentence of `summary`), for cheap scanning.
    /// `None` on rows written before tiered summaries; readers derive it on the fly.
    pub summary_l0: Option<String>,
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
    /// Direct-child file count (0 for files).
    pub file_count: i64,
    /// Total chunk count for all entries under this path (0 for files).
    pub chunk_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    pub pending: i64,
    pub in_flight: i64,
    pub done: i64,
    pub failed: i64,
}

#[derive(Debug, Clone)]
pub struct FailedQueueItem {
    pub path: String,
    pub error: Option<String>,
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

    fn fts_row_count(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn reindex_replaces_chunks_and_keeps_fts_in_sync() {
        let mut store = Store::open_in_memory().unwrap();
        // Two files so the chunks table has several rowids — the old `INSERT OR REPLACE`
        // bug only orphaned FTS rows when the replaced chunk was not the table's max rowid.
        store
            .upsert_chunks(&[
                dummy_chunk("/a.txt", 0, "alpha keep"),
                dummy_chunk("/a.txt", 1, "beta middle"),
                dummy_chunk("/a.txt", 2, "gamma tail"),
                dummy_chunk("/b.txt", 0, "delta other"),
            ])
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 4);
        assert_eq!(fts_row_count(&store), 4);

        // Re-index /a.txt shrunk from 3 chunks down to 1.
        store
            .upsert_chunks(&[dummy_chunk("/a.txt", 0, "alpha keep updated")])
            .unwrap();

        // /a.txt now has exactly 1 chunk; /b.txt untouched → 2 total. FTS must match the
        // chunk count exactly: no orphaned rows, no stale tail chunk left behind.
        assert_eq!(store.chunk_count().unwrap(), 2);
        assert_eq!(
            fts_row_count(&store),
            2,
            "FTS rows must equal chunk rows after a shrinking re-index (no orphans)"
        );

        // The removed tail content must no longer be searchable.
        let gamma = store
            .hybrid_search("gamma", None, &HybridMode::Sparse, None, 10, 60.0)
            .unwrap();
        assert!(gamma.is_empty(), "stale tail chunk 'gamma' should be gone");

        // The surviving file is still searchable.
        let delta = store
            .hybrid_search("delta", None, &HybridMode::Sparse, None, 10, 60.0)
            .unwrap();
        assert_eq!(delta.len(), 1);
        assert!(delta[0].entry_path.contains("/b.txt"));
    }

    #[test]
    fn delete_subtree_clears_summaries_and_queue() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&dummy_summary("/docs/a.txt", "file", Some("/docs"), 1))
            .unwrap();
        store
            .upsert_summary(&dummy_summary("/docs/b.txt", "file", Some("/docs"), 1))
            .unwrap();
        store
            .upsert_summary(&dummy_summary("/other/c.txt", "file", Some("/other"), 1))
            .unwrap();
        store
            .enqueue_summary_items(&[
                ("/docs/a.txt".to_owned(), "file".to_owned(), 1),
                ("/other/c.txt".to_owned(), "file".to_owned(), 1),
            ])
            .unwrap();

        store.delete_subtree("/docs/").unwrap();

        assert!(store.summary_by_path("/docs/a.txt").unwrap().is_none());
        assert!(store.summary_by_path("/docs/b.txt").unwrap().is_none());
        assert!(
            store.summary_by_path("/other/c.txt").unwrap().is_some(),
            "summary outside the deleted subtree must survive"
        );
        let stats = store.queue_stats().unwrap();
        assert_eq!(
            stats.pending, 1,
            "the /docs queue row must be cleared; /other remains"
        );
    }

    #[test]
    fn delete_entry_clears_summary_and_queue() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&dummy_summary("/notes.txt", "file", None, 0))
            .unwrap();
        store
            .enqueue_summary_items(&[("/notes.txt".to_owned(), "file".to_owned(), 0)])
            .unwrap();

        store.delete_entry("/notes.txt").unwrap();

        assert!(store.summary_by_path("/notes.txt").unwrap().is_none());
        assert_eq!(store.queue_stats().unwrap().pending, 0);
    }

    #[test]
    fn unknown_extensions_uses_basename_extension() {
        let mut store = Store::open_in_memory().unwrap();
        // A directory containing a dot plus a multi-dot filename: the old SQL sliced from
        // the FIRST dot in the whole path; the fix must use the true basename extension.
        store
            .upsert_entries(&[
                dummy_entry("/home/user.name/report.tar.gz", EntryKind::File, 1),
                dummy_entry("/home/user.name/notes.gz", EntryKind::File, 1),
                dummy_entry("/home/user.name/README", EntryKind::File, 1),
            ])
            .unwrap();

        let exts = store.unknown_extensions(10).unwrap();
        let gz = exts
            .iter()
            .find(|(e, _)| e == ".gz")
            .expect(".gz must be detected");
        assert_eq!(gz.1, 2, ".gz should count both .tar.gz and .gz files");
        assert!(exts.iter().any(|(e, n)| e == "(no extension)" && *n == 1));
        assert!(
            !exts.iter().any(|(e, _)| e.contains('/')),
            "must not produce a directory-fragment 'extension'"
        );
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
            summary_l0: None,
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
        // upsert_summary derives and persists an L0 abstract even when the record
        // was constructed with summary_l0 = None.
        assert_eq!(got.summary_l0.as_deref(), Some("summary of /docs/file.txt"));
    }

    #[test]
    fn abstract_from_takes_first_sentence_and_caps_length() {
        // First sentence only.
        assert_eq!(
            abstract_from("This is the gist. More detail follows here."),
            "This is the gist."
        );
        // No sentence terminator → whole (short) string.
        assert_eq!(abstract_from("Just a label"), "Just a label");
        // Long single sentence is truncated with an ellipsis on a char boundary.
        let long = "x".repeat(200);
        let got = abstract_from(&long);
        assert!(got.ends_with('…'));
        assert!(got.chars().count() <= 121);
        // Does not panic on multibyte content.
        let _ = abstract_from("Café déjà vu — 日本語 résumé. second");
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
    fn requeue_stale_in_flight_resets_then_caps() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .enqueue_summary_items(&[("/a".to_owned(), "file".to_owned(), 1)])
            .unwrap();

        // Claim → attempts=1, in_flight (simulates a crash mid-processing).
        store.next_queue_item().unwrap().unwrap();
        assert_eq!(store.queue_stats().unwrap().in_flight, 1);

        // Below the cap → requeued to pending.
        assert_eq!(store.requeue_stale_in_flight(3).unwrap(), (1, 0));
        assert_eq!(store.queue_stats().unwrap().pending, 1);

        store.next_queue_item().unwrap().unwrap(); // attempts=2
        assert_eq!(store.requeue_stale_in_flight(3).unwrap(), (1, 0));
        store.next_queue_item().unwrap().unwrap(); // attempts=3 — reaches cap

        // At the cap → failed instead of requeued (it keeps crashing).
        assert_eq!(store.requeue_stale_in_flight(3).unwrap(), (0, 1));
        assert_eq!(store.queue_stats().unwrap().failed, 1);
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
