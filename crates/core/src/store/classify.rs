//! Semantic-classification (Smart classification) reads and writes.
//!
//! A *second axis* over the technical `hint_cat`: a path can carry a semantic
//! category (work/personal/archive/media/code/system/other) the user confirms,
//! corrects, or ignores. Stored in its own `classifications` table so a rescan —
//! which `INSERT OR REPLACE`s `entries` — never wipes a user's label.

use super::types::ClassificationRecord;
use super::Store;
use anyhow::Result;
use rusqlite::params;

fn row_to_classification(r: &rusqlite::Row) -> rusqlite::Result<ClassificationRecord> {
    Ok(ClassificationRecord {
        path: r.get(0)?,
        kind: r.get(1)?,
        category: r.get(2)?,
        // Stored as REAL; read as f64 then narrow to keep f32-FromSql support out of the picture.
        confidence: r.get::<_, f64>(3)? as f32,
        source: r.get(4)?,
        confirmed_at: r.get(5)?,
        created_at: r.get(6)?,
    })
}

impl Store {
    // ── Tier 0 inputs (content-free reads) ────────────────────────────────────

    /// `(path, hint_cat)` for every directory entry. The directory's own surface
    /// hint is the highest-priority Tier 0 signal (e.g. `node_modules` →
    /// build-artifact, `~/Library` → system).
    pub fn dir_entries_with_hint(&self) -> Result<Vec<(String, Option<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, hint_cat FROM entries WHERE kind = 'dir'")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Per-directory histogram of direct-child **file** hint categories:
    /// `(parent_path, hint_cat, count)`. Lets Tier 0 classify a folder by the
    /// dominant category of the files it directly contains.
    pub fn child_file_hint_histogram(&self) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT parent_path, hint_cat, COUNT(*)
               FROM entries
              WHERE kind = 'file' AND parent_path IS NOT NULL AND hint_cat IS NOT NULL
              GROUP BY parent_path, hint_cat",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Writes ────────────────────────────────────────────────────────────────

    /// Insert/refresh a batch of **auto** classifications in one transaction.
    /// `rows`: `(path, kind, category, confidence)`.
    ///
    /// The `WHERE classifications.source = 'auto'` guard on the upsert means a
    /// user-confirmed (`user`) or dismissed (`ignored`) row for the same path is
    /// left untouched: re-running classify only refreshes rows the user has not
    /// decided on.
    pub fn upsert_auto_classifications(
        &mut self,
        rows: &[(String, String, String, f32)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO classifications (path, kind, category, confidence, source)
                 VALUES (?1, ?2, ?3, ?4, 'auto')
                 ON CONFLICT(path) DO UPDATE SET
                     kind       = excluded.kind,
                     category   = excluded.category,
                     confidence = excluded.confidence,
                     created_at = unixepoch()
                 WHERE classifications.source = 'auto'",
            )?;
            for (path, kind, category, confidence) in rows {
                stmt.execute(params![path, kind, category, *confidence as f64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Confirm (or correct) a path's classification as a user decision: a
    /// `source='user'`, full-confidence row that later auto passes never
    /// overwrite. A corrected `category` overrides whatever was auto-suggested.
    pub fn confirm_classification(&self, path: &str, category: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO classifications (path, kind, category, confidence, source, confirmed_at)
             VALUES (?1,
                     COALESCE((SELECT kind FROM classifications WHERE path = ?1), 'dir'),
                     ?2, 1.0, 'user', unixepoch())
             ON CONFLICT(path) DO UPDATE SET
                 category     = excluded.category,
                 confidence   = 1.0,
                 source       = 'user',
                 confirmed_at = unixepoch()",
            params![path, category],
        )?;
        Ok(())
    }

    /// Dismiss a suggestion with a sticky `ignored` tombstone, so it is not
    /// re-proposed on the next classify run.
    pub fn ignore_classification(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO classifications (path, kind, category, confidence, source)
             VALUES (?1,
                     COALESCE((SELECT kind FROM classifications WHERE path = ?1), 'dir'),
                     COALESCE((SELECT category FROM classifications WHERE path = ?1), ''),
                     0.0, 'ignored')
             ON CONFLICT(path) DO UPDATE SET source = 'ignored', confirmed_at = unixepoch()",
            params![path],
        )?;
        Ok(())
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    /// List classifications, optionally filtered to one `source` (`auto` =
    /// pending suggestions, `user` = confirmed). Ordered by descending confidence
    /// then path. `limit` of 0 means no limit.
    pub fn list_classifications(
        &self,
        source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ClassificationRecord>> {
        let lim: i64 = if limit == 0 { -1 } else { limit as i64 };
        let mut stmt = self.conn.prepare(
            "SELECT path, kind, category, confidence, source, confirmed_at, created_at
               FROM classifications
              WHERE (?1 IS NULL OR source = ?1)
              ORDER BY confidence DESC, path
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![source_filter, lim], row_to_classification)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The classification for one exact path, if any.
    pub fn classification_for(&self, path: &str) -> Result<Option<ClassificationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, kind, category, confidence, source, confirmed_at, created_at
               FROM classifications WHERE path = ?1",
        )?;
        let mut rows = stmt.query_map(params![path], row_to_classification)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Total number of classification rows (any source).
    pub fn classification_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM classifications", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Delete a classification row entirely, reverting the path to "no suggestion".
    ///
    /// Used by the Undo action: after deletion, the next `indexa classify` run will
    /// re-surface the Tier-0 auto suggestion as a fresh `source='auto'` row.
    pub fn delete_classification(&mut self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM classifications WHERE path = ?1", params![path])?;
        Ok(())
    }
}
