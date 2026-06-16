//! Hierarchical summary reads/writes and the L0-abstract derivation.

use super::search::{blob_to_embedding, embedding_to_blob, like_prefix};
use super::{Store, SummaryRecord};
use crate::text::snippet;
use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};
use sha2::{Digest, Sha256};
use std::io::Read;

/// Full-content SHA-256 of the file at `path`, lowercase hex. Streams in 64 KiB
/// reads so a 100 MB file never loads into RAM. Returns `""` when the file can't
/// be read — an empty hash means "freshness unknown" and must never enable a
/// skip (and never block the summary itself).
pub fn file_source_hash(path: &std::path::Path) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            // A mid-read failure leaves a partial digest — discard it; a wrong
            // hash (enabling a false skip later) is worse than no hash.
            Err(_) => return String::new(),
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Merkle-style roll-up hash for a directory: SHA-256 over the sorted
/// (child path, child source_hash) pairs, so a dir's hash changes iff its
/// subtree's content or membership did — without touching the disk.
///
/// Returns `""` when there are no children or any child's hash is empty
/// (legacy/unreadable rows): an unknown child means the dir's freshness is
/// unknown, and it must re-roll rather than skip on a hash it can't trust.
pub fn dir_source_hash(children: &[SummaryRecord]) -> String {
    if children.is_empty() || children.iter().any(|c| c.source_hash.is_empty()) {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = children
        .iter()
        .map(|c| (c.path.as_str(), c.source_hash.as_str()))
        .collect();
    // children_summaries orders dirs-then-files; sort by path so the hash is
    // independent of the caller's row order.
    pairs.sort_unstable();
    let mut hasher = Sha256::new();
    for (path, hash) in pairs {
        hasher.update(path.as_bytes());
        hasher.update([0u8]); // NUL separators: ("ab","c") must hash unlike ("a","bc")
        hasher.update(hash.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
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
    // Cap at 120 Unicode characters; `snippet` appends "…" when truncated.
    snippet(first, 120).into_owned()
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
    /// All summary rows, **without embeddings** (the blobs are large and model-specific —
    /// snapshot export omits them). `embedding` is `None` on every returned record.
    pub fn all_summaries(&self) -> Result<Vec<SummaryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, kind, parent_path, depth, summary, summary_l0,
                    child_count, byte_size, model, source_hash, generated_at
             FROM summaries ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SummaryRecord {
                path: r.get(0)?,
                kind: r.get(1)?,
                parent_path: r.get(2)?,
                depth: r.get(3)?,
                summary: r.get(4)?,
                summary_l0: r.get(5)?,
                embedding: None,
                child_count: r.get(6)?,
                byte_size: r.get(7)?,
                model: r.get(8)?,
                source_hash: r.get(9)?,
                generated_at: r.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

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

    /// Restore a summary row's TEXT from a Decision-Ledger stash (summary_drift →
    /// `restore_old`) and NULL its embedding. Returns `false` when no row exists.
    ///
    /// Why the embedding is cleared rather than kept or regenerated: the row's
    /// stored vector embeds the NEW (rejected) wording — leaving it would make
    /// dense retrieval rank the restored text by a summary the user explicitly
    /// rejected, which is dishonest. The projection has no embedder (effects run
    /// in store-only contexts: CLI answer, web answer, crash repair), and
    /// re-enqueueing the path would just regenerate the same drifting summary
    /// and re-ask the question. So the embedding goes NULL: dense retrieval
    /// skips the row until the path is next regenerated for real (content
    /// change or an explicit Regenerate). Acceptable because the summary TEXT
    /// is what users, exports, and FTS read. `source_hash`/`generated_at` stay
    /// untouched — the restored text describes the same bytes, and keeping the
    /// hash stops the freshness gate from immediately re-running the very
    /// regeneration the user rejected.
    /// Known residues, fixed by the next genuine refresh rather than here:
    /// the provenance row still describes the REJECTED regeneration (provider/
    /// passes), and a parent roll-up that already consumed the drifted text is
    /// not re-queued — both converge on the next summarize pass.
    pub fn restore_summary_text(
        &mut self,
        path: &str,
        summary: &str,
        summary_l0: Option<&str>,
        model: Option<&str>,
    ) -> Result<bool> {
        let l0 = summary_l0
            .map(str::to_owned)
            .unwrap_or_else(|| abstract_from(summary));
        let n = self.conn.execute(
            "UPDATE summaries
                SET summary = ?2, summary_l0 = ?3, embedding = NULL,
                    model = COALESCE(?4, model)
              WHERE path = ?1",
            params![path, summary, l0, model],
        )?;
        Ok(n > 0)
    }

    /// Stamp provenance onto an existing summary row (v0.21): which adapter produced it,
    /// how many refinement passes actually ran, and whether a lighter model was
    /// auto-substituted for the configured one. Kept off `SummaryRecord` on purpose —
    /// write-path only for now; read surfaces arrive with the decision ledger.
    pub fn set_summary_provenance(
        &mut self,
        path: &str,
        provider: &str,
        passes: i64,
        fallback: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE summaries SET provider = ?2, passes = ?3, fallback = ?4 WHERE path = ?1",
            params![path, provider, passes, fallback as i64],
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

    /// Delete one summary row by exact path (no children). Returns rows removed.
    pub fn delete_summary(&mut self, path: &str) -> Result<usize> {
        self.conn
            .execute("DELETE FROM summaries WHERE path = ?1", params![path])
            .map_err(Into::into)
    }

    /// Does a summary row exist for `path`? (Cheap existence probe — avoids
    /// materializing the embedding blob that `summary_by_path` would fetch.)
    pub fn summary_exists(&self, path: &str) -> Result<bool> {
        self.conn
            .prepare_cached("SELECT 1 FROM summaries WHERE path = ?1")?
            .exists(params![path])
            .map_err(Into::into)
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

    /// All (path, kind, depth) entries under `root`, including those already in the queue.
    ///
    /// Unlike [`entries_for_summarization`] (which uses `INSERT OR IGNORE` and skips existing
    /// rows), this method is used by force-requeue: every item — even ones already `done` or
    /// `failed` — will be reset to `pending` via `mark_for_resummary`.
    pub fn entries_for_resummary(&self, root: &str) -> Result<Vec<(String, String, i64)>> {
        let pattern = like_prefix(root);
        let mut stmt = self.conn.prepare(
            "SELECT path, kind FROM entries
             WHERE (path LIKE ?1 ESCAPE '\\' OR parent_path LIKE ?1 ESCAPE '\\')
               AND (deep_policy IS NULL OR deep_policy != 'Skip')",
        )?;
        let rows = stmt.query_map(params![pattern], |r| {
            let path: String = r.get(0)?;
            let kind: String = r.get(1)?;
            let depth = path.chars().filter(|&c| c == '/' || c == '\\').count() as i64;
            Ok((path, kind, depth))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// (path, kind) of summarized entries under `root` whose on-disk mtime
    /// (`entries.modified_s`, refreshed by scan) is newer than their summary's
    /// `generated_at` — i.e. summaries that *may* be stale.
    ///
    /// A cheap SQL-only pre-filter for incremental re-summarize: the precise gate
    /// is the content hash in the summarize path (a touched-but-identical file
    /// skips there). Dirs are included on purpose — a dir's mtime bumps when a
    /// direct child is created/removed/renamed, which is exactly when its roll-up
    /// (and its ancestors') goes stale without any surviving file changing.
    pub fn stale_summary_candidates(&self, root: &str) -> Result<Vec<(String, String)>> {
        // Boundary-scoped (exact + "root/" children, mirroring delete_subtree):
        // a bare prefix would bleed /projects-archive into /projects, stealing
        // its staleness signal without the matching ancestor propagation.
        let exact = root.trim_end_matches('/');
        let child_pattern = like_prefix(&format!("{exact}/"));
        // `>=`, not `>`: summarize stamps generated_at at the START of its run,
        // so an edit landing the same second (or during the LLM call) stays
        // flagged. False positives are free — the content-hash gate skips them.
        let mut stmt = self.conn.prepare(
            "SELECT e.path, e.kind FROM entries e
             JOIN summaries s ON s.path = e.path
             WHERE (e.path = ?1 OR e.path LIKE ?2 ESCAPE '\\')
               AND (e.deep_policy IS NULL OR e.deep_policy != 'Skip')
               AND e.modified_s IS NOT NULL
               AND e.modified_s >= s.generated_at",
        )?;
        let rows = stmt.query_map(params![exact, child_pattern], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Blank the stored source hashes for `root`'s subtree (exact + children).
    /// An explicit "Regenerate" must defeat the freshness gate even for
    /// byte-identical content (model/prompt/passes changes, user intent) —
    /// clearing the hash IS the force signal, so the gate itself stays
    /// parameter-free. Returns rows cleared.
    pub fn clear_summary_hashes_under(&mut self, root: &str) -> Result<usize> {
        let exact = root.trim_end_matches('/');
        let child_pattern = like_prefix(&format!("{exact}/"));
        self.conn
            .execute(
                "UPDATE summaries SET source_hash = ''
                  WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
                params![exact, child_pattern],
            )
            .map_err(Into::into)
    }
}
