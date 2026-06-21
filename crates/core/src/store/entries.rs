//! Surface-scan entry writes, counts, and subtree reconciliation/deletion.

use super::search::like_prefix;
use super::types::{EntryInfo, HealthStats};
use super::Store;
use crate::walker::{Entry, EntryKind};
use anyhow::Result;
use rusqlite::{params, OptionalExtension, Transaction};

/// Row type for [`Store::all_coverage_entries`]:
/// `(path, parent_path, is_dir, own_chunk_count, queue_state)`.
pub type CoverageEntry = (String, String, bool, u64, Option<String>);

/// Split a subtree `prefix` into `(exact, child_pattern)` so a delete matches the path
/// itself and everything strictly under it — but NOT a sibling that merely shares the
/// string prefix (`/proj` must not match `/projector`). `child_pattern` is wildcard-escaped
/// for use with `LIKE … ESCAPE '\'`.
pub(super) fn subtree_match(prefix: &str) -> (String, String) {
    let exact = prefix.trim_end_matches('/').to_owned();
    let child_pattern = like_prefix(&format!("{exact}/"));
    (exact, child_pattern)
}

/// Delete chunks (and their FTS5 entries + code-graph edges) for the file at `exact` and
/// every file strictly under `child_pattern`. Shared by `delete_subtree` and
/// `delete_chunks_for_subtree`. Matching the exact path too means deleting a single file's
/// subtree (`/proj/a.rs`) still clears that file's own chunks.
pub(super) fn delete_chunks_under_prefix(
    tx: &Transaction,
    exact: &str,
    child_pattern: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM chunks_fts WHERE entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )?;
    tx.execute(
        "DELETE FROM edges WHERE from_path = ?1 OR from_path LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )?;
    tx.execute(
        "DELETE FROM chunks WHERE entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )
}

/// Hard-delete every artifact (chunks + FTS + edges + summaries + queue + classification +
/// dir-apps + the entry itself) for an EXACT set of paths, returning the number of `entries`
/// rows removed. Batched `IN (…)` per table, chunked under SQLite's bound-variable cap so an
/// arbitrarily large ghost set stays safe. The child tables have no FK `ON DELETE CASCADE`
/// (see `store::schema`), so this is the manual-integrity cleanup path used by `reconcile_entries`.
fn delete_path_artifacts_exact(tx: &Transaction, paths: &[String]) -> rusqlite::Result<usize> {
    let mut removed = 0usize;
    for batch in paths.chunks(800) {
        let ph = vec!["?"; batch.len()].join(",");
        // (table, scoping column) — `edges` keys on `from_path`, the rest on `path`/`entry_path`.
        for (table, col) in [
            ("chunks_fts", "entry_path"),
            ("chunks", "entry_path"),
            ("edges", "from_path"),
            ("summaries", "path"),
            ("summary_queue", "path"),
            ("classifications", "path"),
            ("directory_apps", "path"),
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE {col} IN ({ph})"),
                rusqlite::params_from_iter(batch.iter()),
            )?;
        }
        removed += tx.execute(
            &format!("DELETE FROM entries WHERE path IN ({ph})"),
            rusqlite::params_from_iter(batch.iter()),
        )?;
    }
    Ok(removed)
}

impl Store {
    // ── Surface-scan writes ───────────────────────────────────────────────────

    /// Insert or update a batch of walker entries.
    ///
    /// Uses a non-destructive `ON CONFLICT … DO UPDATE` (not `INSERT OR REPLACE`) so an
    /// existing row keeps its identity across rescans: REPLACE would DELETE then INSERT,
    /// pointlessly churning the row (and resetting `first_indexed_at`). There is no FK
    /// `ON DELETE CASCADE` on the child tables — see the integrity note in `store::schema`.
    pub fn upsert_entries(&mut self, entries: &[Entry]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            // first_indexed_at is set once on INSERT and never overwritten on rescan.
            let mut stmt = tx.prepare_cached(
                "INSERT INTO entries
                 (path, parent_path, kind, size, modified_s, hint_label, hint_cat, deep_policy,
                  first_indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch())
                 ON CONFLICT(path) DO UPDATE SET
                     parent_path = excluded.parent_path,
                     kind        = excluded.kind,
                     size        = excluded.size,
                     modified_s  = excluded.modified_s,
                     hint_label  = excluded.hint_label,
                     hint_cat    = excluded.hint_cat,
                     deep_policy = excluded.deep_policy,
                     indexed_at  = unixepoch()",
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

    /// Look up a single entry's display facts (kind/size/mtime) by exact path. Powers
    /// `indexa inspect`. Returns `None` when the path isn't indexed.
    pub fn entry_by_path(&self, path: &str) -> Result<Option<EntryInfo>> {
        self.conn
            .query_row(
                "SELECT kind, size, modified_s FROM entries WHERE path = ?1",
                params![path],
                |r| {
                    Ok(EntryInfo {
                        kind: r.get::<_, String>(0)?,
                        size: r.get::<_, i64>(1)? as u64,
                        modified_s: r.get::<_, Option<i64>>(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
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
        tx.execute("DELETE FROM directory_apps WHERE path = ?1", params![path])?;
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
        let removed = delete_path_artifacts_exact(&tx, &ghosts)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Remove the entry at `prefix` and all entries strictly under it (a whole directory
    /// subtree), along with their chunks, summaries, and any queued summary work. Matches the
    /// exact path + `prefix/%` so a sibling sharing the string prefix (`/proj` vs `/projector`)
    /// is never touched. Returns the number of `entries` rows deleted.
    pub fn delete_subtree(&mut self, prefix: &str) -> Result<usize> {
        let (exact, child) = subtree_match(prefix);
        let tx = self.conn.transaction()?;
        delete_chunks_under_prefix(&tx, &exact, &child)?;
        // Summaries + queue must be cleared too (symmetry across all tables); an orphaned
        // summary_queue row would otherwise block re-summarization if the path is re-indexed.
        tx.execute(
            "DELETE FROM summaries
              WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'
                 OR parent_path = ?1 OR parent_path LIKE ?2 ESCAPE '\\'",
            params![exact, child],
        )?;
        tx.execute(
            "DELETE FROM summary_queue WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![exact, child],
        )?;
        tx.execute(
            "DELETE FROM classifications WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![exact, child],
        )?;
        tx.execute(
            "DELETE FROM directory_apps WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![exact, child],
        )?;
        let n = tx.execute(
            "DELETE FROM entries
              WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'
                 OR parent_path = ?1 OR parent_path LIKE ?2 ESCAPE '\\'",
            params![exact, child],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Return the indexed root paths — indexed directory entries whose parent is
    /// not itself an indexed entry. These are the top-level nodes for the tree
    /// view (e.g. the project root the user indexed); their real filesystem
    /// parent lives outside the index, so they anchor the roll-up.
    ///
    /// Note: this returns the indexed root *entry* itself (`e1.path`), not its
    /// un-indexed filesystem parent — passing the parent to `summary_by_path`
    /// would miss the root summary and walk away from the data.
    pub fn root_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT e1.path
               FROM entries e1
              WHERE e1.kind = 'dir'
                AND NOT EXISTS (
                    SELECT 1 FROM entries e2 WHERE e2.path = e1.parent_path
                )
              ORDER BY e1.path",
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

    /// File paths whose recorded mtime (`modified_s`) is at or after `cutoff_secs`
    /// (a Unix timestamp). Backs `indexa export --changed-since`. Entries with a NULL
    /// mtime are excluded (we can't claim they changed within the window). Files only —
    /// directories don't carry a meaningful content mtime for recency slicing.
    pub fn paths_modified_since(&self, cutoff_secs: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM entries
              WHERE kind = 'file' AND modified_s IS NOT NULL AND modified_s >= ?1",
        )?;
        let rows = stmt.query_map([cutoff_secs], |r| r.get(0))?;
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

    /// Coverage-oriented flat list for the context-coverage treemap.
    ///
    /// Returns `(path, parent_path, is_dir, own_chunk_count, queue_state)` for each entry.
    ///
    /// - `own_chunk_count`: for files, the count of their indexed chunks; for dirs, 0
    ///   (the treemap builder propagates chunk counts up the tree).
    /// - `queue_state`: the entry's own row in `summary_queue` (`None` when absent).
    ///
    /// Capped at 500,000 rows. The correlated chunk subquery is acceptable at typical
    /// index sizes (thousands of files, each resolved in microseconds).
    pub fn all_coverage_entries(&self) -> Result<Vec<CoverageEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.path,
                    COALESCE(e.parent_path, '') AS parent,
                    e.kind,
                    CASE WHEN e.kind = 'file' THEN
                      (SELECT COUNT(*) FROM chunks WHERE entry_path = e.path)
                    ELSE 0 END AS chunk_count,
                    sq.state
             FROM entries e
             LEFT JOIN summary_queue sq ON sq.path = e.path
             LIMIT 500000",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)? == "dir",
                r.get::<_, i64>(3)? as u64,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Aggregate coverage statistics for the coverage table view.
    ///
    /// Returns counts of directories grouped by their summary queue state:
    /// `(total_dirs, built, partial, failed, none, total_chunks, total_files)`.
    pub fn coverage_stats(&self) -> Result<(u64, u64, u64, u64, u64, u64, u64)> {
        // rusqlite's FromSql is not implemented for u64; use i64 and cast.
        let total_dirs =
            self.conn
                .query_row("SELECT COUNT(*) FROM entries WHERE kind = 'dir'", [], |r| {
                    r.get::<_, i64>(0)
                })? as u64;
        let total_files = self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE kind = 'file'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let total_chunks = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get::<_, i64>(0))?
            as u64;
        let built = self.conn.query_row(
            "SELECT COUNT(*) FROM summary_queue WHERE state = 'done' AND kind = 'dir'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let partial = self.conn.query_row(
            "SELECT COUNT(*) FROM summary_queue WHERE state IN ('pending','in_flight') AND kind = 'dir'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let failed = self.conn.query_row(
            "SELECT COUNT(*) FROM summary_queue WHERE state = 'failed' AND kind = 'dir'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let none = total_dirs.saturating_sub(built + partial + failed);
        Ok((
            total_dirs,
            built,
            partial,
            failed,
            none,
            total_chunks,
            total_files,
        ))
    }

    /// Whole-index coverage aggregates for the `status --deep` health report.
    /// One SELECT of scalar subqueries — no per-row work in Rust. Chunk and
    /// summary counts join back to `entries` so orphan rows left by a removed
    /// root (cleaned by `prune`) never inflate a coverage ratio past 100%.
    /// The stale count compares `summaries.generated_at` to the entry's
    /// on-disk mtime (`modified_s`): older means the file changed after its
    /// summary was written.
    pub fn health_stats(&self) -> Result<HealthStats> {
        self.conn
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM entries WHERE kind = 'file'),
                   (SELECT COUNT(*) FROM entries WHERE kind = 'dir'),
                   (SELECT COUNT(DISTINCT c.entry_path) FROM chunks c
                      JOIN entries e ON e.path = c.entry_path AND e.kind = 'file'),
                   (SELECT COUNT(*) FROM chunks),
                   (SELECT COUNT(*) FROM chunks WHERE embedding IS NOT NULL),
                   (SELECT COUNT(*) FROM summaries s
                      JOIN entries e ON e.path = s.path WHERE s.kind = 'file'),
                   (SELECT COUNT(*) FROM summaries s
                      JOIN entries e ON e.path = s.path WHERE s.kind = 'dir'),
                   (SELECT COUNT(*) FROM summaries s
                      JOIN entries e ON e.path = s.path
                     WHERE e.modified_s IS NOT NULL AND s.generated_at < e.modified_s)",
                [],
                |r| {
                    Ok(HealthStats {
                        files: r.get::<_, i64>(0)? as u64,
                        dirs: r.get::<_, i64>(1)? as u64,
                        files_with_chunks: r.get::<_, i64>(2)? as u64,
                        chunks: r.get::<_, i64>(3)? as u64,
                        embedded_chunks: r.get::<_, i64>(4)? as u64,
                        files_summarized: r.get::<_, i64>(5)? as u64,
                        dirs_summarized: r.get::<_, i64>(6)? as u64,
                        stale_summaries: r.get::<_, i64>(7)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }
}
