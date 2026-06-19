//! Application/structure recognition reads and writes (v0.66).
//!
//! Which known software/stack/structure a *directory* is — Rust crate, Next.js app, macOS `.app`
//! bundle, Terraform module, … — derived by matching file-pattern signatures (see
//! [`crate::fingerprint`]). Stored in the `directory_apps` table, one or more rows per directory
//! (`is_primary` flags the most-specific winner). Fully machine-derived and re-derivable from the
//! tree, so — unlike `classifications`, which can hold a user decision — these rows are cleared by
//! the entry-delete paths and the prune orphan sweep (the classifications lifecycle).

use super::entries::subtree_match;
use super::Store;
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
        let (exact, child) = subtree_match(prefix);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM directory_apps
              WHERE is_primary = 1 AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')
              ORDER BY path"
        ))?;
        let rows = stmt.query_map(params![exact, child], row_to_app)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
