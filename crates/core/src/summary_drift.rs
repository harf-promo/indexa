//! Summary drift classification (v0.78): read-only comparison of the LAST `summarize` pass
//! (the `summaries` table) against the indexed filesystem state (`entries`), bucketed into
//! `stale` / `orphaned` / `uncovered`.
//!
//! **Distinct from two existing, similarly-named concepts — do not conflate:**
//! - [`crate::store::Store::find_stale_entries`] (MCP `insights_stale`) is directory mtime
//!   AGE — "hasn't changed in N days", with no reference to whether anything was ever
//!   summarized at all.
//! - `indexa_query::staleness::is_stale` (used by `ask`) is PER-CITATION chunk freshness,
//!   keyed on `chunks.indexed_at` vs. a live `fs::metadata` read — it answers "is this cited
//!   chunk safe to trust right now", not "does the summarize pass need to run again".
//!
//! This module answers a third question — **summarize-pass coverage**: for every indexed
//! path, does its `summaries` row (if any) still describe what's on disk? It reads the
//! store's already-recorded state (`entries.modified_s`, `summaries.generated_at`), the same
//! data `find_stale_entries` reads, so it needs no live filesystem access and no new schema.

use crate::store::Store;
use anyhow::Result;

/// How one indexed path's summary coverage compares to the last `summarize` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftKind {
    /// A summary exists, but the entry's on-disk content changed (per `entries.modified_s`)
    /// after that summary was generated (`summaries.generated_at`).
    Stale,
    /// A summary row's path is no longer present in `entries` — the file or folder was
    /// deleted or moved since it was summarized.
    Orphaned,
    /// The path is indexed but has no `summaries` row at all — never summarized.
    Uncovered,
}

impl DriftKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DriftKind::Stale => "stale",
            DriftKind::Orphaned => "orphaned",
            DriftKind::Uncovered => "uncovered",
        }
    }
}

/// One path's drift classification.
#[derive(Debug, Clone)]
pub struct DriftEntry {
    pub path: String,
    /// `"file"` | `"dir"`.
    pub kind: String,
    pub drift: DriftKind,
    /// Unix seconds the summary was generated — `None` for [`DriftKind::Uncovered`].
    pub summary_generated_at: Option<i64>,
    /// Unix seconds the entry was last observed modified — `None` for [`DriftKind::Orphaned`]
    /// (the entry is gone) or when the entry has never recorded an mtime.
    pub modified_s: Option<i64>,
}

/// The full drift report, bucketed. Each bucket is sorted by path for stable output.
#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    pub stale: Vec<DriftEntry>,
    pub orphaned: Vec<DriftEntry>,
    pub uncovered: Vec<DriftEntry>,
}

impl DriftReport {
    pub fn is_empty(&self) -> bool {
        self.stale.is_empty() && self.orphaned.is_empty() && self.uncovered.is_empty()
    }

    pub fn total(&self) -> usize {
        self.stale.len() + self.orphaned.len() + self.uncovered.len()
    }
}

/// Classify every indexed path against the last `summarize` pass. Two queries (rather than a
/// `FULL OUTER JOIN`, unsupported on the SQLite versions this ships against): entries
/// LEFT JOIN summaries covers stale + uncovered, summaries LEFT JOIN entries covers orphaned.
pub fn drift_report(store: &Store) -> Result<DriftReport> {
    let mut report = DriftReport::default();

    {
        let mut stmt = store.db_connection().prepare(
            "SELECT e.path, e.kind, e.modified_s, s.generated_at
               FROM entries e
               LEFT JOIN summaries s ON s.path = e.path
              ORDER BY e.path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        for row in rows {
            let (path, kind, modified_s, generated_at) = row?;
            match generated_at {
                None => report.uncovered.push(DriftEntry {
                    path,
                    kind,
                    drift: DriftKind::Uncovered,
                    summary_generated_at: None,
                    modified_s,
                }),
                Some(g) => {
                    if let Some(m) = modified_s {
                        if m > g {
                            report.stale.push(DriftEntry {
                                path,
                                kind,
                                drift: DriftKind::Stale,
                                summary_generated_at: Some(g),
                                modified_s,
                            });
                        }
                    }
                }
            }
        }
    }

    {
        let mut stmt = store.db_connection().prepare(
            "SELECT s.path, s.kind, s.generated_at
               FROM summaries s
               LEFT JOIN entries e ON e.path = s.path
              WHERE e.path IS NULL
              ORDER BY s.path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (path, kind, generated_at) = row?;
            report.orphaned.push(DriftEntry {
                path,
                kind,
                drift: DriftKind::Orphaned,
                summary_generated_at: Some(generated_at),
                modified_s: None,
            });
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SummaryRecord;

    /// Insert a bare `entries` row directly — `walker::Entry` carries scan-time fields
    /// (`SystemTime`, hint detection) this module has no reason to depend on; the drift
    /// query only ever reads `path`, `kind`, `modified_s`.
    fn seed_entry(store: &Store, path: &str, kind: &str, modified_s: Option<i64>) {
        store
            .db_connection()
            .execute(
                "INSERT INTO entries (path, kind, modified_s) VALUES (?1, ?2, ?3)",
                rusqlite::params![path, kind, modified_s],
            )
            .unwrap();
    }

    fn summary(path: &str, kind: &str, generated_at: i64) -> SummaryRecord {
        SummaryRecord {
            path: path.to_owned(),
            kind: kind.to_owned(),
            parent_path: None,
            depth: 0,
            summary: "a summary".to_owned(),
            summary_l0: None,
            embedding: None,
            child_count: 0,
            byte_size: 0,
            model: "m".to_owned(),
            source_hash: "h".to_owned(),
            generated_at,
        }
    }

    #[test]
    fn classifies_stale_orphaned_and_uncovered() {
        let mut store = Store::open_in_memory().unwrap();
        // Summarized, then changed on disk after — stale.
        seed_entry(&store, "/r/stale.rs", "file", Some(200));
        // Summarized, never touched since — not reported at all.
        seed_entry(&store, "/r/fresh.rs", "file", Some(50));
        // Indexed, never summarized — uncovered.
        seed_entry(&store, "/r/new.rs", "file", Some(10));
        store
            .upsert_summary(&summary("/r/stale.rs", "file", 100))
            .unwrap();
        store
            .upsert_summary(&summary("/r/fresh.rs", "file", 100))
            .unwrap();
        // Summarized once, then the file vanished from the index — orphaned.
        store
            .upsert_summary(&summary("/r/gone.rs", "file", 100))
            .unwrap();

        let report = drift_report(&store).unwrap();
        assert_eq!(
            report
                .stale
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/r/stale.rs"]
        );
        assert_eq!(report.stale[0].drift, DriftKind::Stale);
        assert_eq!(
            report
                .uncovered
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/r/new.rs"]
        );
        assert_eq!(
            report
                .orphaned
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/r/gone.rs"]
        );
        assert_eq!(report.total(), 3);
        assert!(!report.is_empty());
    }

    #[test]
    fn empty_index_reports_nothing() {
        let store = Store::open_in_memory().unwrap();
        let report = drift_report(&store).unwrap();
        assert!(report.is_empty());
        assert_eq!(report.total(), 0);
    }

    #[test]
    fn a_summary_generated_after_the_last_observed_mtime_is_not_stale() {
        let mut store = Store::open_in_memory().unwrap();
        seed_entry(&store, "/r/f.rs", "file", Some(50));
        // Re-summarized after the file's last recorded mtime — current, not stale.
        store
            .upsert_summary(&summary("/r/f.rs", "file", 500))
            .unwrap();
        let report = drift_report(&store).unwrap();
        assert!(report.is_empty());
    }
}
