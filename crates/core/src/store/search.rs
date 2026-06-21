//! Hybrid / cosine search and the shared FTS / embedding encoding helpers.

use super::{AnnIndex, RegionSummary, SearchHit, Store, TreeNode};
use crate::config::HybridMode;
use rusqlite::{params, Row};

use anyhow::Result;

// ── Encoding/decoding helpers ─────────────────────────────────────────────────

/// Encode an `f32` vector as a little-endian byte blob (4 bytes per f32).
pub(super) fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode a little-endian byte blob back into an `f32` vector.
///
/// Any trailing bytes that don't form a complete 4-byte chunk are silently
/// dropped (via `chunks_exact`), matching the historical behavior at the
/// summary call sites. Callers that need strict alignment validation should
/// check `b.len().is_multiple_of(4)` before calling.
pub(super) fn blob_to_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Wrap a user query in FTS5 double-quote syntax so arbitrary natural-language
/// text (with punctuation, `?`, parentheses, etc.) is treated as a phrase search
/// rather than triggering FTS5 query-syntax errors.
pub(super) fn fts5_quote(s: &str) -> String {
    // Escape any embedded double-quotes by doubling them.
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A tiny English stopword set — words that carry no retrieval signal but, left in
/// the query, would dilute BM25 ranking with matches on ubiquitous words. Kept small
/// and in-code (no dependency). English-only, which fits Indexa's mostly-English /
/// code corpus; non-English content still matches via its content terms.
const FTS_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "could", "do", "does", "for", "from",
    "how", "i", "in", "into", "is", "it", "its", "me", "my", "not", "of", "on", "or", "our",
    "should", "that", "the", "this", "to", "was", "we", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

/// Build the FTS5 MATCH expression for a raw user query.
///
/// Tokenizes on non-alphanumeric boundaries, lowercases, drops stopwords + 1-char
/// tokens, quotes each remaining term, and emits `"<whole query>" OR "t1" OR "t2" …`:
/// an exact-phrase hit (adjacent tokens) still scores highest via `bm25(chunks_fts)`,
/// while the OR'd terms add recall for multi-word natural-language questions. This
/// replaces wrapping the WHOLE query as a single FTS5 phrase, which only matched a
/// near-verbatim adjacent token run — so a question like "how does the watcher
/// reindex" returned almost nothing in sparse mode. A single content term needs no
/// phrase; an all-stopword / punctuation-only query falls back to the phrase form so
/// a query never silently degrades to "no results". The same expression feeds the
/// lexical (BM25) arm of `rrf` too, so hybrid `ask`/`search` gain the recall as well.
pub(super) fn build_fts_query(query: &str) -> String {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .filter(|t| t.chars().count() > 1 && !FTS_STOPWORDS.contains(&t.as_str()))
        .map(|t| fts5_quote(&t))
        .collect();
    match terms.len() {
        // No content terms survived (all stopwords / punctuation) — keep the old
        // whole-query phrase behavior rather than emit an empty (error) MATCH.
        0 => fts5_quote(query),
        // A single content word: the phrase and the term are identical — skip the OR.
        1 => terms.into_iter().next().unwrap(),
        // Phrase (exact-adjacency boost) OR the individual terms (recall).
        _ => format!("{} OR {}", fts5_quote(query), terms.join(" OR ")),
    }
}

/// Escape `%` and `_` wildcards in a path prefix before appending `%` for LIKE matching.
/// Must be used with `LIKE ?n ESCAPE '\'` in the SQL clause.
pub(super) fn like_prefix(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

/// SQL `WHERE`-fragment (begins with ` AND `) that excludes content-free stub chunks from
/// retrieval — the `File: <name>` / `Image: …` / `Media file: …` placeholders a binary,
/// image, or media parser emits when it has nothing real to extract. They carry no semantic
/// content but embed near generic "what is this file?" queries and crowd out real content.
/// Kept in sync with [`is_stub_chunk`]: same prefixes + the same <80-char length cap (a real
/// document that happens to start "File: …" runs far longer). Works on any table exposing a
/// `text` column (`chunks`, `chunks_fts`).
pub(super) const STUB_EXCLUDE_SQL: &str =
    " AND NOT (length(text) < 80 AND (text LIKE 'File: %' OR text LIKE 'Image: %' OR text LIKE 'Media file: %'))";

/// Whether `text` is a content-free parser stub (see [`STUB_EXCLUDE_SQL`]). The Rust-side
/// guard for any stub that still reaches `retrieve()` — e.g. via the ANN dense arm, which
/// returns ids from the HNSW index without running the SQL filter.
pub fn is_stub_chunk(text: &str) -> bool {
    text.len() < 80
        && (text.starts_with("File: ")
            || text.starts_with("Image: ")
            || text.starts_with("Media file: "))
}

pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

/// Map a row from the `entries` + `summary_queue` join (used by `search_paths`
/// and `tree_level`) into a `TreeNode`.
/// Column order: path, kind, size, file_count, chunk_count, summary_state,
/// subtree_covered, subtree_partial, subtree_total
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
        covered: r.get::<_, i64>(6).unwrap_or(0),
        partial: r.get::<_, i64>(7).unwrap_or(0),
        total: r.get::<_, i64>(8).unwrap_or(0),
    })
}

impl Store {
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
        self.hybrid_search_with_ann(query_text, query_embedding, mode, scope, limit, rrf_k, None)
    }

    /// `hybrid_search` plus an optional ANN index for the dense arm. When `ann` is `Some`
    /// and the query is unscoped, dense candidates come from the HNSW index instead of a
    /// brute-force cosine scan; any miss (scoped query, dimension mismatch, empty result)
    /// falls back to brute-force, so results are unchanged — only faster at scale.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_with_ann(
        &self,
        query_text: &str,
        query_embedding: Option<&[f32]>,
        mode: &HybridMode,
        scope: Option<&str>,
        limit: usize,
        rrf_k: f32,
        ann: Option<&AnnIndex>,
    ) -> Result<Vec<SearchHit>> {
        let rrf_k = rrf_k as f64;

        // ── FTS5 keyword retrieval ────────────────────────────────────────────
        let fts_candidates: Vec<(i64, String)> = match mode {
            HybridMode::Dense => Vec::new(),
            _ => {
                let fts_query = build_fts_query(query_text);
                let (sql, scope_param) = if let Some(s) = scope {
                    (
                        format!(
                            "SELECT CAST(chunk_id AS INTEGER), entry_path, bm25(chunks_fts) AS score
                             FROM chunks_fts
                             WHERE chunks_fts MATCH ?1 AND entry_path LIKE ?2 ESCAPE '\\'{STUB_EXCLUDE_SQL}
                             ORDER BY score LIMIT 100"
                        ),
                        Some(like_prefix(s)),
                    )
                } else {
                    (
                        format!(
                            "SELECT CAST(chunk_id AS INTEGER), entry_path, bm25(chunks_fts) AS score
                             FROM chunks_fts
                             WHERE chunks_fts MATCH ?1{STUB_EXCLUDE_SQL}
                             ORDER BY score LIMIT 100"
                        ),
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
                    self.dense_candidates(qvec, 100, scope, ann)?
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

    /// Search entries whose path contains `query` (case-insensitive LIKE).
    pub fn search_paths(&self, query: &str, limit: usize) -> Result<Vec<TreeNode>> {
        let pattern = format!("%{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind, e.size,
                    (SELECT COUNT(*) FROM entries c
                     WHERE c.parent_path = e.path AND c.kind = 'file') AS file_count,
                    (SELECT COUNT(*) FROM chunks
                     WHERE entry_path LIKE e.path || '/%') AS chunk_count,
                    sq.state AS summary_state,
                    (SELECT COUNT(*) FROM summary_queue q
                     WHERE q.kind = 'dir' AND q.state = 'done'
                       AND (q.path = e.path OR q.path LIKE e.path || '/%')) AS subtree_covered,
                    (SELECT COUNT(*) FROM summary_queue q
                     WHERE q.kind = 'dir' AND q.state IN ('pending','in_flight')
                       AND (q.path = e.path OR q.path LIKE e.path || '/%')) AS subtree_partial,
                    (SELECT COUNT(*) FROM entries d
                     WHERE d.kind = 'dir'
                       AND (d.path = e.path OR d.path LIKE e.path || '/%')) AS subtree_total
               FROM entries e
               LEFT JOIN summary_queue sq ON sq.path = e.path
              WHERE e.path LIKE ?1
              ORDER BY LENGTH(e.path) ASC, e.path ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], row_to_tree_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// One level of the tree: entries under `parent_path` with their summary states.
    ///
    /// An empty `parent_path` lists the indexed *roots* (directory entries whose
    /// parent is not itself indexed) rather than entries with a literal empty
    /// parent — no row carries an empty `parent_path`, so the old `= ?1` form
    /// returned nothing for the first-load / `browse_tree("")` case.
    ///
    /// Performance: this is the web `api_tree` / MCP `browse_tree` hot path. The
    /// reference (`tree_level_reference`, the `#[cfg(test)]` correctness oracle)
    /// attached four O(subtree) prefix-`LIKE` correlated subqueries to *every*
    /// child row — a dir with `C` children cost ~`4·C` full subtree scans. This
    /// implementation instead fetches the children once, then runs ONE GROUP-free
    /// streaming scan per subtree metric (`chunks`, `summary_queue` dir rows,
    /// `entries` dir rows) scoped to `parent_path`'s subtree and buckets each row
    /// to its owning child in Rust by longest-prefix match — total work
    /// ~`O(children + subtree-rows)` instead of `O(children · subtree)`.
    ///
    /// The bucketing is provably equivalent to the reference: every direct child
    /// of `parent_path` is mutually non-nested (no child's path is a prefix-at-a-
    /// `/`-boundary of another's), so a subtree path `P` is owned by AT MOST ONE
    /// child `c` — the unique one with `P == c.path` or `P` starting with
    /// `c.path + "/"` — exactly the reference's `(q.path = c.path OR q.path LIKE
    /// c.path || '/%')`. Filters are preserved byte-for-byte: `chunk_count`
    /// counts descendants only (`LIKE c.path || '/%'`, NO self match, NO kind
    /// filter); covered/partial count `summary_queue` rows with `kind='dir'` in
    /// the `done` / (`pending`|`in_flight`) state sets, self-or-descendant;
    /// `subtree_total` counts `entries` rows with `kind='dir'`, self-or-descendant;
    /// `file_count` is the direct-child file count. Ordering (`kind DESC, path`)
    /// and `TreeNode` field population match `row_to_tree_node`.
    pub fn tree_level(&self, parent_path: &str) -> Result<Vec<TreeNode>> {
        use std::collections::HashMap;

        // ── 1. Fetch the children of `parent_path` (same WHERE as the reference,
        //        incl. the root case) + the two cheap per-row facts: the LEFT-JOINed
        //        summary state and the direct-child file count. Both stay as small
        //        correlated subqueries / joins — they are O(1)-ish per child, not
        //        O(subtree), so they were never the bottleneck.
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind, e.size,
                    (SELECT COUNT(*) FROM entries c
                     WHERE c.parent_path = e.path AND c.kind = 'file') AS file_count,
                    sq.state AS summary_state
             FROM entries e
             LEFT JOIN summary_queue sq ON sq.path = e.path
             WHERE (?1 <> '' AND e.parent_path = ?1)
                OR (?1 = '' AND e.kind = 'dir'
                    AND NOT EXISTS (SELECT 1 FROM entries p WHERE p.path = e.parent_path))
             ORDER BY e.kind DESC, e.path",
        )?;
        let mut nodes: Vec<TreeNode> = stmt
            .query_map(params![parent_path], |r| {
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
                    chunk_count: 0,
                    child_count: 0,
                    summary_state: r.get(4)?,
                    covered: 0,
                    partial: 0,
                    total: 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        if nodes.is_empty() {
            return Ok(nodes);
        }

        // ── 2. Index children by path → position in `nodes` for O(1) bucket merge.
        //        Keys are OWNED (cloned) so the maps don't borrow `nodes` — we mutate
        //        `nodes[i]` while bucketing.
        let child_paths: Vec<String> = nodes.iter().map(|n| n.path.clone()).collect();
        let child_index: HashMap<String, usize> = child_paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();

        // The subtree to scan: `parent_path` and everything under it, expressed as a
        // `LIKE` prefix (`"<parent>/%"`). For the root case (`parent_path == ""`) this
        // is `like_prefix("/")` = `"/%"`, i.e. "every path beginning with `/`". On POSIX
        // that IS the whole table (absolute paths all start with `/`), and bucketing then
        // drops rows under no fetched child (e.g. a root's un-indexed FS parent). NOTE:
        // this — like the `b'/'` boundary check in `bucket_of` and every other prefix-LIKE
        // path query in the store — assumes `/`-separated stored paths. Windows native
        // paths (`C:\proj\…`, stored verbatim) don't start with `/` and don't use `/`
        // boundaries, so their subtree rollups under-count. Fixing that uniformly is a
        // storage-layer concern (normalize separators at index time), not a per-query
        // patch; tracked separately, not addressed here.
        let subtree_like = like_prefix(&format!("{parent_path}/"));

        // Resolve a subtree path `P` to the index of the child it belongs to (the
        // unique child `c` with `P == c.path` or `P` startswith `c.path + "/"`), or
        // `None` if it sits under no fetched child. Children are mutually non-nested,
        // so checking each candidate prefix is unambiguous.
        let bucket_of = |p: &str| -> Option<usize> {
            // Exact match (the `q.path = c.path` arm of the reference).
            if let Some(&i) = child_index.get(p) {
                return Some(i);
            }
            // Descendant match (the `q.path LIKE c.path || '/%'` arm): the owning
            // child is the unique one whose path + '/' prefixes `p`.
            for (i, c) in child_paths.iter().enumerate() {
                if p.len() > c.len()
                    && p.as_bytes()[c.len()] == b'/'
                    && p.as_bytes()[..c.len()] == *c.as_bytes()
                {
                    return Some(i);
                }
            }
            None
        };

        // ── 3a. chunk_count — ALL chunks under each child (descendants only:
        //         `LIKE c.path || '/%'`, NO self, NO kind filter). The reference
        //         scoped this with `entry_path LIKE e.path || '/%'`; here we scan the
        //         parent subtree once and bucket. A chunk exactly AT a child's own
        //         path (entry_path == c.path) is NOT a descendant and is excluded —
        //         matching the reference, which had no `= e.path` arm for chunks.
        {
            let mut s = self
                .conn
                .prepare("SELECT entry_path FROM chunks WHERE entry_path LIKE ?1 ESCAPE '\\'")?;
            let mut rows = s.query(params![subtree_like])?;
            while let Some(row) = rows.next()? {
                let p: String = row.get(0)?;
                // Descendant-only: a chunk whose entry_path equals a child path
                // belongs to no bucket (the reference required a '/%' suffix).
                if child_index.contains_key(p.as_str()) {
                    continue;
                }
                if let Some(i) = bucket_of(&p) {
                    nodes[i].chunk_count += 1;
                }
            }
        }

        // ── 3b. subtree dir rollups from summary_queue (covered = done,
        //         partial = pending|in_flight), self-or-descendant, kind='dir'.
        {
            let mut s = self.conn.prepare(
                "SELECT path, state FROM summary_queue
                 WHERE kind = 'dir'
                   AND state IN ('done','pending','in_flight')
                   AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')",
            )?;
            let mut rows = s.query(params![parent_path, subtree_like])?;
            while let Some(row) = rows.next()? {
                let p: String = row.get(0)?;
                let state: String = row.get(1)?;
                if let Some(i) = bucket_of(&p) {
                    match state.as_str() {
                        "done" => nodes[i].covered += 1,
                        "pending" | "in_flight" => nodes[i].partial += 1,
                        _ => {}
                    }
                }
            }
        }

        // ── 3c. subtree_total — entries dir rows, self-or-descendant, kind='dir'.
        {
            let mut s = self.conn.prepare(
                "SELECT path FROM entries
                 WHERE kind = 'dir' AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')",
            )?;
            let mut rows = s.query(params![parent_path, subtree_like])?;
            while let Some(row) = rows.next()? {
                let p: String = row.get(0)?;
                if let Some(i) = bucket_of(&p) {
                    nodes[i].total += 1;
                }
            }
        }

        Ok(nodes)
    }

    /// Byte-for-byte original of [`tree_level`], kept as the correctness oracle for
    /// the set-based rewrite. The equivalence test asserts the new `tree_level`
    /// returns output identical to this for the root level and several non-root
    /// parents. DO NOT change its SQL.
    #[cfg(test)]
    pub(crate) fn tree_level_reference(&self, parent_path: &str) -> Result<Vec<TreeNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind, e.size,
                    (SELECT COUNT(*) FROM entries c
                     WHERE c.parent_path = e.path AND c.kind = 'file') AS file_count,
                    (SELECT COUNT(*) FROM chunks
                     WHERE entry_path LIKE e.path || '/%') AS chunk_count,
                    sq.state AS summary_state,
                    (SELECT COUNT(*) FROM summary_queue q
                     WHERE q.kind = 'dir' AND q.state = 'done'
                       AND (q.path = e.path OR q.path LIKE e.path || '/%')) AS subtree_covered,
                    (SELECT COUNT(*) FROM summary_queue q
                     WHERE q.kind = 'dir' AND q.state IN ('pending','in_flight')
                       AND (q.path = e.path OR q.path LIKE e.path || '/%')) AS subtree_partial,
                    (SELECT COUNT(*) FROM entries d
                     WHERE d.kind = 'dir'
                       AND (d.path = e.path OR d.path LIKE e.path || '/%')) AS subtree_total
             FROM entries e
             LEFT JOIN summary_queue sq ON sq.path = e.path
             WHERE (?1 <> '' AND e.parent_path = ?1)
                OR (?1 = '' AND e.kind = 'dir'
                    AND NOT EXISTS (SELECT 1 FROM entries p WHERE p.path = e.parent_path))
             ORDER BY e.kind DESC, e.path",
        )?;
        let rows = stmt.query_map(params![parent_path], row_to_tree_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
            let ext = std::path::Path::new(&path)
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
        // Scan the top-50 (not top-20) summaries by cosine so a deep subtree summary that
        // ranks past 20 still boosts its chunks — cheap recall gain (only runs when
        // summary_weight > 0, which is off by default).
        let summaries = self.summary_cosine_search(query_vec, 50, depth_alpha)?;
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

    /// Brute-force cosine similarity over all stored embeddings.
    /// Returns (chunk_id, entry_path) sorted by descending similarity.
    fn cosine_search(
        &self,
        query: &[f32],
        limit: usize,
        scope: Option<&str>,
    ) -> Result<Vec<(i64, String)>> {
        let sql = if scope.is_some() {
            format!("SELECT id, entry_path, embedding FROM chunks WHERE embedding IS NOT NULL AND entry_path LIKE ?1 ESCAPE '\\'{STUB_EXCLUDE_SQL}")
        } else {
            format!("SELECT id, entry_path, embedding FROM chunks WHERE embedding IS NOT NULL{STUB_EXCLUDE_SQL}")
        };
        let mut stmt = self.conn.prepare(&sql)?;

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

    /// Dense top-`limit` candidates as `(chunk_id, entry_path)`, via the ANN index when one
    /// is supplied and the query is unscoped (HNSW returns global neighbours with no path
    /// filter), else a brute-force cosine scan. An ANN miss (empty result, e.g. a dimension
    /// mismatch) also falls back, so dense recall is never worse than brute-force.
    fn dense_candidates(
        &self,
        query: &[f32],
        limit: usize,
        scope: Option<&str>,
        ann: Option<&AnnIndex>,
    ) -> Result<Vec<(i64, String)>> {
        if scope.is_none() {
            if let Some(index) = ann {
                let ids = index.search(query, limit);
                if !ids.is_empty() {
                    let resolved = self.paths_for_ids(&ids)?;
                    // Fall back when nothing resolved (e.g. every id was deleted since the
                    // index was built) rather than contributing zero dense candidates.
                    if !resolved.is_empty() {
                        return Ok(resolved);
                    }
                }
            }
        }
        self.cosine_search(query, limit, scope)
    }

    /// Resolve `(id, entry_path)` for chunk ids in order, skipping ids no longer present
    /// (an ANN node can outlive its chunk row between rebuilds — dropping it is safe).
    ///
    /// One batched `IN (…)` query instead of N `query_row`s; results are re-ordered back to the
    /// input `ids` order in Rust (SQLite `IN` returns no ordering guarantee). Behaviour matches the
    /// old per-id loop exactly — same paths, same order, missing ids skipped, duplicate input ids
    /// preserved. `ids` is the ANN result set, bounded by the retrieval limit, so it stays well
    /// under SQLite's variable cap.
    fn paths_for_ids(&self, ids: &[i64]) -> Result<Vec<(i64, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT id, entry_path FROM chunks WHERE id IN ({placeholders})");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut by_id: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, path) = row?;
            by_id.insert(id, path);
        }
        Ok(ids
            .iter()
            .filter_map(|&id| by_id.get(&id).map(|p| (id, p.clone())))
            .collect())
    }

    /// All `(chunk_id, embedding)` pairs with an embedding — the input to building an
    /// [`AnnIndex`](super::AnnIndex).
    pub fn all_chunk_embeddings(&self) -> Result<Vec<(i64, Vec<f32>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding FROM chunks WHERE embedding IS NOT NULL")?;
        let rows = stmt.query_map([], |r| {
            let id: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            Ok((id, blob))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            if blob.len().is_multiple_of(4) {
                out.push((id, blob_to_embedding(&blob)));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{build_fts_query, fts5_quote};

    #[test]
    fn build_fts_query_phrase_or_terms_for_multiword() {
        // Stopwords (how/does/the) dropped; the whole-query phrase is kept for the
        // exact-adjacency BM25 boost; the surviving content terms are OR'd for recall.
        assert_eq!(
            build_fts_query("How does the watcher reindex?"),
            "\"How does the watcher reindex?\" OR \"watcher\" OR \"reindex\""
        );
    }

    #[test]
    fn build_fts_query_single_content_term_is_bare() {
        // One content word → just the quoted term, no redundant phrase/OR.
        assert_eq!(build_fts_query("sqlite"), "\"sqlite\"");
        // Stopwords around a single content word collapse to that term.
        assert_eq!(build_fts_query("what is sqlite"), "\"sqlite\"");
    }

    #[test]
    fn build_fts_query_drops_stopwords_and_one_char_tokens() {
        // "a" (stopword) + "x" (1-char) dropped; "redact" + "secrets" kept.
        assert_eq!(
            build_fts_query("redact a x secrets"),
            "\"redact a x secrets\" OR \"redact\" OR \"secrets\""
        );
    }

    #[test]
    fn build_fts_query_all_stopwords_falls_back_to_phrase() {
        // No content terms survive → fall back to the quoted phrase, never an empty MATCH.
        assert_eq!(build_fts_query("how is it"), fts5_quote("how is it"));
    }

    #[test]
    fn build_fts_query_punctuation_or_empty_falls_back() {
        assert_eq!(build_fts_query("???"), fts5_quote("???"));
        assert_eq!(build_fts_query(""), fts5_quote(""));
    }

    #[test]
    fn build_fts_query_escapes_embedded_quotes() {
        // An embedded `"` is a token boundary AND is doubled inside the phrase by
        // fts5_quote, so the emitted MATCH stays syntactically valid.
        assert_eq!(
            build_fts_query("say \"hi\" now"),
            "\"say \"\"hi\"\" now\" OR \"say\" OR \"hi\" OR \"now\""
        );
    }
}
