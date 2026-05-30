//! Hierarchical summary reads/writes and the L0-abstract derivation.

use super::search::{blob_to_embedding, embedding_to_blob, like_prefix};
use super::{Store, SummaryRecord};
use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};

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

impl Store {
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

    /// Count of summary rows.
    pub fn summary_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))?;
        Ok(n as u64)
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
}
