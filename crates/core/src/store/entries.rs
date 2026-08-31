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
///
/// Separator-agnostic by inference, not by matching both: a given index is
/// separator-homogeneous by construction (every stored path comes from one machine's
/// `PathBuf::to_string_lossy()` at write time, below — so within one index, paths are
/// consistently `/`-separated or consistently `\`-separated, never mixed, barring a
/// caller-supplied mixed-separator string). So the separator used to build
/// `child_pattern` is taken from `prefix` itself: the last `/` or `\` in it, defaulting to
/// `/` when it has none (a bare root or a single path segment). See `search.rs`'s
/// module docs for why this beats normalize-at-write-time + migrate, and why matching
/// both separators via `OR`-ed LIKE patterns or `REPLACE(...)` was rejected.
///
/// The separator is read from the ORIGINAL `prefix`, before trimming — not from `exact`.
/// For a bare drive root (`C:\`) the trailing separator IS the only separator in the
/// string; inferring from the already-trimmed `exact` (`C:`) would find none and
/// silently default to `/`, reproducing this exact bug's failure shape for that one
/// input. Every other case is unaffected: a trailing separator is itself a valid
/// instance of the separator, so trimming never changes what `rfind` would have seen.
pub(super) fn subtree_match(prefix: &str) -> (String, String) {
    let sep = prefix
        .rfind(['/', '\\'])
        .map_or('/', |i| prefix.as_bytes()[i] as char);
    let exact = prefix.trim_end_matches(['/', '\\']).to_owned();
    let child_pattern = like_prefix(&format!("{exact}{sep}"));
    (exact, child_pattern)
}

/// [`subtree_match`], but an empty `prefix` means "match every path" instead of only a
/// literal empty-string path (which no row has). Several callers use an empty prefix as
/// their documented "no scope restriction" value (batch review's `under=""`, the whole-index
/// module list) — `subtree_match("")` alone would silently narrow those to nothing, since
/// its `child_pattern` becomes `/%` (requires a leading slash) rather than "everything."
pub(super) fn subtree_match_or_all(prefix: &str) -> (String, String) {
    if prefix.is_empty() {
        (String::new(), "%".to_owned())
    } else {
        subtree_match(prefix)
    }
}

/// Delete chunks (and their FTS5 entries + code-graph edges + symbols) for the file at
/// `exact` and every file strictly under `child_pattern`. Shared by `delete_subtree` and
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
        "DELETE FROM symbols WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )?;
    tx.execute(
        "DELETE FROM note_anchors WHERE note_path = ?1 OR note_path LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )?;
    tx.execute(
        "DELETE FROM co_change WHERE path_a = ?1 OR path_a LIKE ?2 ESCAPE '\\'
                                    OR path_b = ?1 OR path_b LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )?;
    tx.execute(
        "DELETE FROM chunks WHERE entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\'",
        params![exact, child_pattern],
    )
}

/// (table, scoping column) for every single-column entry-keyed child table — `edges` keys on
/// `from_path`, `note_anchors` on `note_path`, the rest on `path`/`entry_path`. Shared by
/// `delete_path_artifacts_exact` (exact-path deletes) and `delete_generation_ghosts`
/// (generation-scoped deletes, D5) so the two delete cascades can never drift out of sync with
/// each other — a table missing from either would silently leave orphans (there is no FK `ON
/// DELETE CASCADE`; see the integrity note in `store::schema`). `co_change` is NOT in this list:
/// it's keyed on a PAIR of columns (`path_a`/`path_b`), so each caller below handles it separately.
const ENTRY_CHILD_TABLES: &[(&str, &str)] = &[
    ("chunks_fts", "entry_path"),
    ("chunks", "entry_path"),
    ("edges", "from_path"),
    ("symbols", "path"),
    ("note_anchors", "note_path"),
    ("summaries", "path"),
    ("summary_queue", "path"),
    ("classifications", "path"),
    ("directory_apps", "path"),
];

/// Hard-delete every artifact (chunks + FTS + edges + symbols + note anchors + co_change +
/// summaries + queue + classification + dir-apps + the entry itself) for an EXACT set of paths,
/// returning the number of `entries` rows removed. Batched `IN (…)` per table, chunked under
/// SQLite's bound-variable cap so an arbitrarily large ghost set stays safe. The child tables have
/// no FK `ON DELETE CASCADE` (see `store::schema`), so this is the manual-integrity cleanup path
/// used by `reconcile_entries`.
fn delete_path_artifacts_exact(tx: &Transaction, paths: &[String]) -> rusqlite::Result<usize> {
    let mut removed = 0usize;
    for batch in paths.chunks(800) {
        let ph = vec!["?"; batch.len()].join(",");
        for &(table, col) in ENTRY_CHILD_TABLES {
            tx.execute(
                &format!("DELETE FROM {table} WHERE {col} IN ({ph})"),
                rusqlite::params_from_iter(batch.iter()),
            )?;
        }
        // co_change is keyed on a PAIR of paths — neither column alone matches the
        // uniform single-column loop above.
        tx.execute(
            &format!("DELETE FROM co_change WHERE path_a IN ({ph}) OR path_b IN ({ph})",),
            rusqlite::params_from_iter(batch.iter().chain(batch.iter())),
        )?;
        removed += tx.execute(
            &format!("DELETE FROM entries WHERE path IN ({ph})"),
            rusqlite::params_from_iter(batch.iter()),
        )?;
    }
    Ok(removed)
}

/// Delete every artifact (the same set as [`delete_path_artifacts_exact`] — see
/// [`ENTRY_CHILD_TABLES`]) for `entries` rows under a root subtree whose `scan_generation` is NOT
/// the current scan's — either an older generation or NULL (a watch upsert, or a pre-D5-migration
/// row this scan did not re-see). The streaming-scan (D5) analogue of `delete_path_artifacts_exact`:
/// subquery-based (no materialized ghost-path list held in memory), so a mostly-deleted root stays
/// bounded-memory regardless of how many ghost rows it has. Returns the number of `entries` rows
/// removed.
fn delete_generation_ghosts(
    tx: &Transaction,
    exact: &str,
    child_pattern: &str,
    generation: i64,
) -> rusqlite::Result<usize> {
    // A ghost row's path: under the subtree, and not stamped with the current generation. `?1`/`?2`
    // reuse the same subtree-boundary shape as `subtree_match`'s other consumers; `?3` is reused
    // verbatim everywhere this subquery is inlined below (SQLite numbered params bind once
    // regardless of how many times `?N` appears in the statement text).
    const GHOST_PATHS: &str = "SELECT path FROM entries \
         WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\') \
           AND (scan_generation IS NULL OR scan_generation != ?3)";
    for (table, col) in ENTRY_CHILD_TABLES {
        tx.execute(
            &format!("DELETE FROM {table} WHERE {col} IN ({GHOST_PATHS})"),
            params![exact, child_pattern, generation],
        )?;
    }
    // co_change is keyed on a PAIR of paths — same dual-column handling as
    // `delete_path_artifacts_exact`, reusing the ghost-paths subquery for both sides.
    tx.execute(
        &format!(
            "DELETE FROM co_change WHERE path_a IN ({GHOST_PATHS}) OR path_b IN ({GHOST_PATHS})"
        ),
        params![exact, child_pattern, generation],
    )?;
    tx.execute(
        "DELETE FROM entries \
         WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\') \
           AND (scan_generation IS NULL OR scan_generation != ?3)",
        params![exact, child_pattern, generation],
    )
}

impl Store {
    // ── Surface-scan writes ───────────────────────────────────────────────────

    /// Insert or update a batch of walker entries WITHOUT stamping a scan generation (delegates to
    /// [`Store::upsert_entries_with_generation`] with `generation: None`). Used by the watchers
    /// (per-event upserts) and most tests; a scan run should call `upsert_entries_with_generation`
    /// directly so its rows are stamped for [`Store::reconcile_by_generation`].
    pub fn upsert_entries(&mut self, entries: &[Entry]) -> Result<()> {
        self.upsert_entries_with_generation(entries, None)
    }

    /// Insert or update a batch of walker entries, stamping `entries.scan_generation` when
    /// `generation` is `Some` (a scan run calling [`Store::next_scan_generation`] once up front).
    /// `None` (a watch upsert, or [`Store::upsert_entries`]) leaves an existing row's generation
    /// untouched via `COALESCE` — so a live-file change picked up between full scans doesn't make
    /// the row look stale to the next [`Store::reconcile_by_generation`] sweep.
    ///
    /// Uses a non-destructive `ON CONFLICT … DO UPDATE` (not `INSERT OR REPLACE`) so an
    /// existing row keeps its identity across rescans: REPLACE would DELETE then INSERT,
    /// pointlessly churning the row (and resetting `first_indexed_at`). There is no FK
    /// `ON DELETE CASCADE` on the child tables — see the integrity note in `store::schema`.
    pub fn upsert_entries_with_generation(
        &mut self,
        entries: &[Entry],
        generation: Option<i64>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            // first_indexed_at is set once on INSERT and never overwritten on rescan.
            // scan_generation (?9): stamped as-is on INSERT; on UPDATE, COALESCE keeps the row's
            // existing generation when ?9 is NULL (a watch upsert) and overwrites it when a scan
            // run supplies one — see the doc comment above.
            let mut stmt = tx.prepare_cached(
                "INSERT INTO entries
                 (path, parent_path, kind, size, modified_s, hint_label, hint_cat, deep_policy,
                  scan_generation, first_indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())
                 ON CONFLICT(path) DO UPDATE SET
                     parent_path     = excluded.parent_path,
                     kind            = excluded.kind,
                     size            = excluded.size,
                     modified_s      = excluded.modified_s,
                     hint_label      = excluded.hint_label,
                     hint_cat        = excluded.hint_cat,
                     deep_policy     = excluded.deep_policy,
                     scan_generation = COALESCE(excluded.scan_generation, scan_generation),
                     indexed_at      = unixepoch()",
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
                    generation,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The generation id to stamp on the next scan run: one past the max already stored (0 on an
    /// empty/fresh index, so the first scan stamps generation 1). Call once at the start of a scan
    /// run and reuse the same value across every `upsert_entries_with_generation` +
    /// `reconcile_by_generation` call in that run, so files re-seen this run carry it and ghosts
    /// keep whatever older value (or NULL) they had.
    ///
    /// **INVARIANT: one call per scan run, shared by every root that run scans** — never call this
    /// again per-root within the same run. `MAX(scan_generation)` is computed over the WHOLE table,
    /// not scoped to a root, so calling it a second time mid-run would hand root 2 a HIGHER
    /// generation than root 1 already used. Nothing breaks immediately (each `reconcile_by_generation`
    /// is still subtree-scoped to its own root), but it silently breaks the "one generation = one
    /// consistent snapshot of this run" property a future cross-root reconcile could rely on — e.g.
    /// reconciling a parent root after a child root finished would then see the child's rows as a
    /// stale (lower) generation and wipe them, even though the child was freshly re-scanned this run.
    pub fn next_scan_generation(&self) -> Result<i64> {
        let g: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(scan_generation), 0) + 1 FROM entries",
            [],
            |r| r.get(0),
        )?;
        Ok(g)
    }

    /// Prune ghost rows under `root_prefix`: entries (and all their artifacts) whose
    /// `scan_generation` is not `generation` — i.e. everything the just-completed scan of this root
    /// did NOT re-stamp. The generation-based, bounded-memory replacement for
    /// [`Store::reconcile_entries`]'s live-path `HashSet` diff — no live-path set is held here, so
    /// this stays cheap even under a whole-computer scan. Also **interruption-safe**: a scan killed
    /// mid-way leaves rows stamped at its generation but never calls this, so the survivors just
    /// look stale to the NEXT full scan's `reconcile_by_generation`, which then prunes them —
    /// self-healing without any separate crash-recovery logic. Subtree-scoped via `subtree_match`
    /// so `/proj` never prunes `/projector`. Returns the count of `entries` rows removed.
    pub fn reconcile_by_generation(&mut self, root_prefix: &str, generation: i64) -> Result<usize> {
        let (exact, child_pattern) = subtree_match(root_prefix);
        let tx = self.conn.transaction()?;
        let removed = delete_generation_ghosts(&tx, &exact, &child_pattern, generation)?;
        tx.commit()?;
        Ok(removed)
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
        tx.execute("DELETE FROM symbols WHERE path = ?1", params![path])?;
        tx.execute(
            "DELETE FROM note_anchors WHERE note_path = ?1",
            params![path],
        )?;
        tx.execute(
            "DELETE FROM co_change WHERE path_a = ?1 OR path_b = ?1",
            params![path],
        )?;
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
        // Boundary-scoped: exact root OR paths under `{root}/` — a bare `LIKE root%` also matches a
        // prefix-sibling root (scanning `/a/proj` would reconcile — and delete — `/a/projector`).
        let (exact, child) = subtree_match(root_prefix);
        let indexed_paths: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path FROM entries WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'")?;
            let rows = stmt.query_map(params![exact, child], |r| r.get(0))?;
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

    // ── Content-based category tagging (agent-session content-scope) ─────────

    /// File entries whose path ends `.jsonl`/`.ndjson` and are not yet tagged `hint_cat =
    /// category` (`hint_cat` is NULL, or holds some other value). Candidates for a content-based
    /// re-check — `hint_cat` is a plain, un-migrated string column (see `AGENTS.md`'s invariant
    /// on it), so this is a query helper, not a schema change. Used by
    /// `indexa_query::session_scope::tag_agent_session_entries` to find `.jsonl`/`.ndjson` rows
    /// worth re-checking against the content-sniffed `AgentSessionParser` without re-scanning
    /// the whole index.
    pub fn jsonl_like_entries_not_tagged(&self, category: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM entries \
             WHERE kind = 'file' \
               AND (path LIKE '%.jsonl' OR path LIKE '%.ndjson') \
               AND (hint_cat IS NULL OR hint_cat != ?1)",
        )?;
        let rows = stmt.query_map(params![category], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Stamp `entries.hint_cat = category` for the exact path. Used to record a content-based
    /// classification a rescan's coarse extension/MIME hinting can't express on its own (e.g.
    /// `"agent-session"` for a `.jsonl` transcript confirmed by content-sniffing, not extension).
    pub fn set_entry_category(&mut self, path: &str, category: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET hint_cat = ?1 WHERE path = ?2",
            params![category, path],
        )?;
        Ok(())
    }

    /// Batch-lookup `hint_cat` for a set of paths, skipping rows with a NULL category. Powers
    /// the MCP `search` tool's `category:`/`category` post-hoc hit filter (mirrors the shape of
    /// the existing `ext_filter` retain-block, applied to a store-backed category instead of a
    /// path suffix). Chunked under SQLite's bound-variable cap like
    /// `delete_path_artifacts_exact`, so an arbitrarily large hit set stays safe.
    pub fn hint_cats_for(
        &self,
        paths: &[&str],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::new();
        for batch in paths.chunks(800) {
            let ph = vec!["?"; batch.len()].join(",");
            let mut stmt = self.conn.prepare(&format!(
                "SELECT path, hint_cat FROM entries WHERE path IN ({ph}) AND hint_cat IS NOT NULL"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(batch.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (path, cat) = row?;
                out.insert(path, cat);
            }
        }
        Ok(out)
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

#[cfg(test)]
mod tests {
    use super::subtree_match;

    #[test]
    fn unix_separator_inferred_and_boundary_preserved() {
        let (exact, child) = subtree_match("/proj");
        assert_eq!(exact, "/proj");
        assert_eq!(child, "/proj/%");
        // The boundary character itself must be a real `/`, not swallowed into a
        // prefix-sibling match: `/proj/%` does not match `/projector`.
        assert!(!like_matches(&child, "/projector"));
        assert!(like_matches(&child, "/proj/a.rs"));
    }

    #[test]
    fn windows_separator_inferred_from_prefix() {
        let (exact, child) = subtree_match(r"C:\proj");
        assert_eq!(exact, r"C:\proj");
        // Escaped for `LIKE … ESCAPE '\'`: every literal `\` is doubled, including the
        // freshly-appended separator (see the exact-escaping test below).
        assert_eq!(child, r"C:\\proj\\%");
        assert!(!like_matches(&child, r"C:\projector"));
        assert!(like_matches(&child, r"C:\proj\a.rs"));
    }

    /// Pinned exact-escaping assertion (not just "some backslashes somewhere"): every
    /// stored `\` — including the freshly-appended separator — is doubled by
    /// `like_escape` for the SQL `ESCAPE '\'` clause, then a single bare `%` wildcard is
    /// appended. Guards against double-escaping (escaping the separator twice) or
    /// under-escaping (leaving a bare `\` SQLite would try to interpret as an escape
    /// prefix for the next character instead of a literal path separator).
    #[test]
    fn windows_prefix_escapes_every_backslash_including_the_appended_separator() {
        let (exact, child) = subtree_match(r"C:\dev\indexa");
        assert_eq!(exact, r"C:\dev\indexa");
        assert_eq!(child, r"C:\\dev\\indexa\\%");
    }

    #[test]
    fn trailing_separator_trimmed_for_both_styles() {
        assert_eq!(subtree_match("/proj/"), subtree_match("/proj"));
        assert_eq!(subtree_match(r"C:\proj\"), subtree_match(r"C:\proj"));
    }

    #[test]
    fn no_separator_in_prefix_defaults_to_forward_slash() {
        // A bare single-segment prefix (no `/` or `\` anywhere) has nothing to infer
        // from — default to `/`, matching every other empty-prefix/root convention in
        // the store (`subtree_match_or_all`, `code_graph_scoped`'s `/` whole-disk case).
        let (exact, child) = subtree_match("proj");
        assert_eq!(exact, "proj");
        assert_eq!(child, "proj/%");
    }

    #[test]
    fn windows_drive_root_still_infers_backslash() {
        // Regression: a bare drive root's only separator IS the trailing one that
        // `exact` trims away. Inferring the separator from `exact` (post-trim) instead
        // of `prefix` (pre-trim) would find no separator left to look at and silently
        // fall back to `/`, reproducing this whole bug's failure shape for exactly the
        // input a Windows "whole disk" scope would pass.
        let (exact, child) = subtree_match(r"C:\");
        assert_eq!(exact, "C:");
        assert_eq!(child, r"C:\\%");
        assert!(like_matches(&child, r"C:\proj"));
        assert!(like_matches(&child, r"C:\proj\a.rs"));
    }

    /// Minimal stand-in for the SQL `LIKE … ESCAPE '\'` semantics `child_pattern` is
    /// built for: `%` is a wildcard, `\%`/`\\`/`\_` are escaped literals. Enough to
    /// assert the two properties the store relies on — sibling exclusion and child
    /// inclusion — without spinning up a real SQLite connection for a pure-Rust helper.
    fn like_matches(pattern: &str, candidate: &str) -> bool {
        let mut lit = String::new();
        let mut chars = pattern.chars().peekable();
        let mut wildcard_tail = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        lit.push(next);
                    }
                }
                '%' if chars.peek().is_none() => wildcard_tail = true,
                other => lit.push(other),
            }
        }
        if wildcard_tail {
            candidate.starts_with(&lit)
        } else {
            candidate == lit
        }
    }

    // ── Content-based category tagging ──────────────────────────────────────

    use super::super::Store;
    use crate::walker::{Entry, EntryKind};

    fn seed_file(store: &mut Store, path: &str) {
        store
            .upsert_entries(&[Entry {
                path: path.into(),
                kind: EntryKind::File,
                size: 0,
                modified: None,
                hint: None,
                is_binary: false,
            }])
            .unwrap();
    }

    #[test]
    fn set_entry_category_and_hint_cats_for_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        seed_file(&mut store, "/p/a.jsonl");
        seed_file(&mut store, "/p/b.jsonl");

        // Untagged rows are absent from a batch lookup, not present with a NULL/empty value.
        let cats = store.hint_cats_for(&["/p/a.jsonl", "/p/b.jsonl"]).unwrap();
        assert!(cats.is_empty());

        store
            .set_entry_category("/p/a.jsonl", "agent-session")
            .unwrap();

        let cats = store.hint_cats_for(&["/p/a.jsonl", "/p/b.jsonl"]).unwrap();
        assert_eq!(
            cats.get("/p/a.jsonl").map(String::as_str),
            Some("agent-session")
        );
        assert!(!cats.contains_key("/p/b.jsonl"));
    }

    #[test]
    fn jsonl_like_entries_not_tagged_finds_untagged_and_differently_tagged_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        seed_file(&mut store, "/p/untagged.jsonl");
        seed_file(&mut store, "/p/already.jsonl");
        seed_file(&mut store, "/p/other.ndjson");
        seed_file(&mut store, "/p/not-jsonl.json"); // wrong extension — never a candidate
        store
            .set_entry_category("/p/already.jsonl", "agent-session")
            .unwrap();

        let mut candidates = store
            .jsonl_like_entries_not_tagged("agent-session")
            .unwrap();
        candidates.sort();
        assert_eq!(
            candidates,
            vec![
                "/p/other.ndjson".to_string(),
                "/p/untagged.jsonl".to_string()
            ]
        );
    }
}
