//! Application/structure recognition reads and writes (v0.66).
//!
//! Which known software/stack/structure a *directory* is — Rust crate, Next.js app, macOS `.app`
//! bundle, Terraform module, … — derived by matching file-pattern signatures (see
//! [`crate::fingerprint`]). Stored in the `directory_apps` table, one or more rows per directory
//! (`is_primary` flags the most-specific winner). Fully machine-derived and re-derivable from the
//! tree, so — unlike `classifications`, which can hold a user decision — these rows are cleared by
//! the entry-delete paths and the prune orphan sweep (the classifications lifecycle).

use super::entries::{subtree_match, subtree_match_or_all};
use super::Store;
use crate::pathutil::norm_sep;
use anyhow::Result;
use rusqlite::params;

/// One detected application/structure at a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedApp {
    pub path: String,
    /// Stable machine id, e.g. "nextjs_app", "macos_app_bundle".
    pub app_kind: String,
    /// Display name, e.g. "Next.js app".
    pub app_name: String,
    /// Taxonomy family: "code" | "os" | "infra" | "data".
    pub family: String,
    pub specificity: u32,
    /// True for the single most-specific match at this directory.
    pub is_primary: bool,
    /// JSON array of the marker strings that defined the match (debug/provenance).
    pub markers_json: String,
    /// "builtin" | "seed:<source>" | "user".
    pub source: String,
    pub detected_at: i64,
}

fn row_to_app(r: &rusqlite::Row) -> rusqlite::Result<DetectedApp> {
    Ok(DetectedApp {
        path: r.get(0)?,
        app_kind: r.get(1)?,
        app_name: r.get(2)?,
        family: r.get(3)?,
        specificity: r.get::<_, i64>(4)? as u32,
        is_primary: r.get::<_, i64>(5)? != 0,
        markers_json: r.get(6)?,
        source: r.get(7)?,
        detected_at: r.get(8)?,
    })
}

const COLS: &str =
    "path, app_kind, app_name, family, specificity, is_primary, markers_json, source, detected_at";

impl Store {
    /// Replace ALL detected-app rows for one directory in a single transaction (delete + insert),
    /// so a re-index that changed what a folder is (or stopped being anything) self-corrects.
    /// Idempotent. The `path` argument is authoritative — each app's `path` field is ignored.
    pub fn replace_apps_for_dir(&mut self, path: &str, apps: &[DetectedApp]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM directory_apps WHERE path = ?1", params![path])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO directory_apps
                    (path, app_kind, app_name, family, specificity, is_primary, markers_json, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for a in apps {
                stmt.execute(params![
                    path,
                    a.app_kind,
                    a.app_name,
                    a.family,
                    a.specificity as i64,
                    a.is_primary as i64,
                    a.markers_json,
                    a.source,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All detected apps for one directory, primary first then most-specific.
    pub fn apps_for_dir(&self, path: &str) -> Result<Vec<DetectedApp>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM directory_apps WHERE path = ?1
             ORDER BY is_primary DESC, specificity DESC, app_name"
        ))?;
        let rows = stmt.query_map(params![path], row_to_app)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The single most-specific detected app for a directory, if any (cheap — partial index).
    pub fn primary_app_for_dir(&self, path: &str) -> Result<Option<DetectedApp>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM directory_apps WHERE path = ?1 AND is_primary = 1 LIMIT 1"
        ))?;
        let mut rows = stmt.query_map(params![path], row_to_app)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Every detected app across the index (for `indexa fingerprint`), grouped-friendly order.
    pub fn all_detected_apps(&self) -> Result<Vec<DetectedApp>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM directory_apps ORDER BY family, app_name, path"
        ))?;
        let rows = stmt.query_map([], row_to_app)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Primary detected apps at or under a path prefix — one row per directory. Used to annotate
    /// the project overview's child-directory lines in a single query.
    pub fn primary_apps_under(&self, prefix: &str) -> Result<Vec<DetectedApp>> {
        // `subtree_match_or_all`, not `subtree_match`: an empty prefix means "no
        // scope restriction" here (the welcome "Build context" list's default,
        // reached via `GET /api/projects` with no `path`). `subtree_match("")`
        // alone produces the `/`-anchored LIKE pattern `/%`, which only matches
        // "everything" by the coincidence that Unix absolute paths start with
        // `/` — on Windows (`C:\...`) it matches nothing, so this call returned
        // an empty project list unconditionally (M7).
        let (exact, child) = subtree_match_or_all(prefix);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM directory_apps
              WHERE is_primary = 1 AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')
              ORDER BY path"
        ))?;
        let rows = stmt.query_map(params![exact, child], row_to_app)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Chunk + summary coverage for one path (the path itself and everything under it).
    /// `covered`/`partial`/`total` are directory-summary rollups — same contract as
    /// `TreeNode` (`store/search.rs`'s `tree_level`/`row_to_tree_node`): `covered` counts
    /// `done` dir summaries, `partial` counts dir summaries still `pending`/`in_flight`
    /// (queued for a build — not necessarily one actively running right now; a
    /// standalone `deep` enqueues rows with no worker draining them until
    /// `summarize`/`index` runs), `total` counts subtree dirs.
    pub fn path_coverage(&self, path: &str) -> Result<PathCoverage> {
        let (exact, child) = subtree_match(path);
        let chunk_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks
             WHERE entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\'",
            params![exact, child],
            |r| r.get(0),
        )?;
        let covered: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM summary_queue
             WHERE kind = 'dir' AND state = 'done'
               AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')",
            params![exact, child],
            |r| r.get(0),
        )?;
        let partial: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM summary_queue
             WHERE kind = 'dir' AND state IN ('pending', 'in_flight')
               AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')",
            params![exact, child],
            |r| r.get(0),
        )?;
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entries
             WHERE kind = 'dir' AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')",
            params![exact, child],
            |r| r.get(0),
        )?;
        Ok(PathCoverage {
            chunk_count: chunk_count as u64,
            has_summary: self.summary_exists(&exact)?,
            covered: covered as u64,
            partial: partial as u64,
            total: total as u64,
        })
    }

    /// Primary detected apps under `prefix`, dropping vendored/generated noise and
    /// nested children of another primary app. The welcome "Build context" list
    /// uses this so a monorepo is one row, not every crate.
    pub fn top_projects_under(&self, prefix: &str) -> Result<Vec<DetectedApp>> {
        let apps = self.primary_apps_under(prefix)?;
        let kept: Vec<DetectedApp> = apps
            .into_iter()
            .filter(|a| !is_project_noise_path(&a.path))
            .collect();
        // Separator-normalize both sides of the nested-child check: a
        // Windows-persisted child path uses `\`, so comparing it against a
        // `/`-suffixed parent prefix without normalizing either side never
        // matches (M7).
        let paths: Vec<String> = kept
            .iter()
            .map(|a| norm_sep(&a.path).into_owned())
            .collect();
        Ok(kept
            .into_iter()
            .zip(paths.iter())
            .filter(|(_, this)| {
                !paths
                    .iter()
                    .any(|other| other != *this && this.starts_with(&format!("{other}/")))
            })
            .map(|(a, _)| a)
            .collect())
    }
}

/// Coverage snapshot for one path — chunks (searchable) vs summaries (understood).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathCoverage {
    pub chunk_count: u64,
    pub has_summary: bool,
    /// Directory summaries in the subtree whose queue state is `done`.
    pub covered: u64,
    /// Directory summaries in the subtree still queued (`pending`/`in_flight`) —
    /// work waiting to be built, distinct from "nothing queued at all". Not a
    /// guarantee that a job is running against it *right now*: a standalone
    /// `deep` enqueues these rows before any `summarize`/`index` job drains them.
    pub partial: u64,
    /// Directory entries in the subtree.
    pub total: u64,
}

/// Detected-app paths that are not useful "Build context" targets: vendored
/// copies, generated trees, nested Xcode project bundles.
pub fn is_project_noise_path(path: &str) -> bool {
    const FRAGMENTS: &[&str] = &[
        "/vendor/",
        "/node_modules/",
        "/target/",
        "/dist/",
        "/assets/vendor/",
        "/.next/",
        "/build/",
        ".xcodeproj",
        "/DerivedData/",
        "/Pods/",
    ];
    // Fragments are `/`-delimited literals; normalize first so a
    // Windows-persisted path (`\`-separated) still matches (M7).
    let normalized = norm_sep(path);
    FRAGMENTS.iter().any(|f| normalized.contains(f))
}
