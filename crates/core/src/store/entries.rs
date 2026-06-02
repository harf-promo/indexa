//! Surface-scan entry writes, counts, and subtree reconciliation/deletion.

use super::search::like_prefix;
use super::Store;
use crate::walker::{Entry, EntryKind};
use anyhow::Result;
use rusqlite::{params, Transaction};

/// Delete chunks (and their FTS5 entries) for every row whose `entry_path`
/// matches the LIKE pattern. Shared by `delete_subtree` and
/// `delete_chunks_for_subtree`.
pub(super) fn delete_chunks_under_prefix(
    tx: &Transaction,
    pattern: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM chunks_fts WHERE entry_path LIKE ?1 ESCAPE '\\'",
        params![pattern],
    )?;
    tx.execute(
        "DELETE FROM edges WHERE from_path LIKE ?1 ESCAPE '\\'",
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
    tx.execute("DELETE FROM edges WHERE from_path = ?1", params![path])?;
    tx.execute("DELETE FROM summaries WHERE path = ?1", params![path])?;
    tx.execute("DELETE FROM summary_queue WHERE path = ?1", params![path])?;
    tx.execute("DELETE FROM classifications WHERE path = ?1", params![path])?;
    tx.execute("DELETE FROM entries WHERE path = ?1", params![path])
}

impl Store {
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

    // ── Queries ───────────────────────────────────────────────────────────────

    /// Count of all indexed entries.
    pub fn entry_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        Ok(n as u64)
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
        // Drop the file's code-graph edges too — else `who_imports`/`dependencies` keep
        // listing a deleted file (this is the live watcher file-removal path).
        tx.execute("DELETE FROM edges WHERE from_path = ?1", params![path])?;
        // Keep the summary tables symmetric with chunks/entries: leaving these behind
        // orphans summary rows and (worse) leaves a stale summary_queue row that
        // `entries_for_summarization` filters on, permanently blocking re-summarization.
        tx.execute("DELETE FROM summaries WHERE path = ?1", params![path])?;
        tx.execute("DELETE FROM summary_queue WHERE path = ?1", params![path])?;
        tx.execute("DELETE FROM classifications WHERE path = ?1", params![path])?;
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
        tx.execute(
            "DELETE FROM classifications WHERE path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        let n = tx.execute(
            "DELETE FROM entries WHERE path LIKE ?1 ESCAPE '\\' OR parent_path LIKE ?1 ESCAPE '\\'",
            params![pattern],
        )?;
        tx.commit()?;
        Ok(n)
    }

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

    /// All indexed entry paths (files and directories). Used by fingerprint detection,
    /// which builds a directory → direct-children map from them.
    pub fn all_entry_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM entries")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Flat list of all entries for building client-side tree visualisations (e.g. treemap).
    /// Returns `(path, parent_path, is_dir, size_bytes)`. Capped at 500,000 rows.
    pub fn all_entry_sizes(&self) -> Result<Vec<(String, String, bool, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, COALESCE(parent_path, ''), kind, size FROM entries LIMIT 500000",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)? == "dir",
                r.get::<_, i64>(3)? as u64,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
