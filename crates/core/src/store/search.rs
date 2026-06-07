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

/// Escape `%` and `_` wildcards in a path prefix before appending `%` for LIKE matching.
/// Must be used with `LIKE ?n ESCAPE '\'` in the SQL clause.
pub(super) fn like_prefix(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
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
    pub fn tree_level(&self, parent_path: &str) -> Result<Vec<TreeNode>> {
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
             WHERE e.parent_path = ?1
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
    fn paths_for_ids(&self, ids: &[i64]) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT entry_path FROM chunks WHERE id = ?1")?;
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            if let Ok(path) = stmt.query_row(params![id], |r| r.get::<_, String>(0)) {
                out.push((id, path));
            }
        }
        Ok(out)
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
