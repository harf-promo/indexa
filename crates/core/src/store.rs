use crate::walker::{Entry, EntryKind};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the index database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating index directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening index at {}", path.display()))?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS entries (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                parent_path TEXT,
                kind        TEXT NOT NULL CHECK(kind IN ('file','dir')),
                size        INTEGER NOT NULL DEFAULT 0,
                modified_s  INTEGER,
                hint_label  TEXT,
                hint_cat    TEXT,
                deep_policy TEXT,
                indexed_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_path);
            CREATE INDEX IF NOT EXISTS idx_entries_kind   ON entries(kind);
            CREATE INDEX IF NOT EXISTS idx_entries_cat    ON entries(hint_cat);
            ",
        )?;
        Ok(())
    }

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

    /// Count of all indexed entries.
    pub fn entry_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        Ok(n as u64)
    }

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
}

#[derive(Debug)]
pub struct RegionSummary {
    pub category: String,
    pub entry_count: u64,
    pub total_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::{Entry, EntryKind};
    use std::path::PathBuf;

    fn dummy_entry(path: &str, kind: EntryKind, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size,
            modified: None,
            hint: None,
        }
    }

    #[test]
    fn open_in_memory_and_upsert() {
        let mut store = Store::open_in_memory().unwrap();
        let entries = vec![
            dummy_entry("/home/user/file.txt", EntryKind::File, 1024),
            dummy_entry("/home/user/docs", EntryKind::Dir, 0),
        ];
        store.upsert_entries(&entries).unwrap();
        assert_eq!(store.entry_count().unwrap(), 2);
    }

    #[test]
    fn upsert_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let e = vec![dummy_entry("/tmp/a.txt", EntryKind::File, 10)];
        store.upsert_entries(&e).unwrap();
        store.upsert_entries(&e).unwrap();
        assert_eq!(store.entry_count().unwrap(), 1);
    }

    #[test]
    fn region_summary_groups_by_category() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[dummy_entry("/a.txt", EntryKind::File, 100)])
            .unwrap();
        let summary = store.region_summary().unwrap();
        assert!(!summary.is_empty());
    }
}
