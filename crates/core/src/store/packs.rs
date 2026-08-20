//! Context Packs: named, cross-directory context bundles.

use super::{PackEvent, PackRecord, Store};
use anyhow::Result;
use rusqlite::params;

impl Store {
    /// Append one row to `pack_events` (4.1). Best-effort in spirit but propagates errors
    /// like any other write — a caller that wants "never fail the primary operation over a
    /// history-write hiccup" should record usage the way `record_usage` does elsewhere, but
    /// pack events are cheap, schema-checked, and always inside the same transaction as the
    /// CRUD they describe, so there's no realistic failure mode worth swallowing here.
    fn record_pack_event(
        &mut self,
        pack_id: &str,
        event: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pack_events (pack_id, event, detail) VALUES (?1, ?2, ?3)",
            params![pack_id, event, detail],
        )?;
        Ok(())
    }

    /// Create a new pack with a unique name. Returns the generated pack ID.
    pub fn create_pack(&mut self, name: &str, description: Option<&str>) -> Result<String> {
        let id: String = self.conn.query_row(
            "INSERT INTO packs (id, name, description)
             VALUES (lower(hex(randomblob(8))), ?1, ?2)
             RETURNING id",
            params![name, description],
            |r| r.get(0),
        )?;
        self.record_pack_event(&id, "created", Some(name))?;
        Ok(id)
    }

    /// Rename a pack. Errors if `new_name` is already taken (UNIQUE name constraint).
    /// Returns the number of rows changed (0 = no pack with that id).
    pub fn rename_pack(&mut self, pack_id: &str, new_name: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE packs SET name = ?1 WHERE id = ?2",
            params![new_name, pack_id],
        )?;
        if n > 0 {
            self.record_pack_event(pack_id, "renamed", Some(new_name))?;
        }
        Ok(n)
    }

    /// Add paths to a pack (idempotent — duplicates are silently ignored).
    pub fn add_pack_paths(&mut self, pack_id: &str, paths: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO pack_paths (pack_id, path) VALUES (?1, ?2)",
            )?;
            for path in paths {
                stmt.execute(params![pack_id, path])?;
            }
        }
        if !paths.is_empty() {
            tx.execute(
                "INSERT INTO pack_events (pack_id, event, detail) VALUES (?1, 'path_added', ?2)",
                params![pack_id, paths.join(", ")],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove specific paths from a pack.
    pub fn remove_pack_paths(&mut self, pack_id: &str, paths: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("DELETE FROM pack_paths WHERE pack_id = ?1 AND path = ?2")?;
            for path in paths {
                stmt.execute(params![pack_id, path])?;
            }
        }
        if !paths.is_empty() {
            tx.execute(
                "INSERT INTO pack_events (pack_id, event, detail) VALUES (?1, 'path_removed', ?2)",
                params![pack_id, paths.join(", ")],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record that a pack was exported (4.1/4.2) — called from the export code paths
    /// (CLI/MCP/web), not from any store-internal CRUD, since exporting isn't a store write
    /// in itself. `detail` is typically the export format (`"xml"`/`"md"`/`"json"`/`"okf"`).
    pub fn record_pack_exported(&mut self, pack_id: &str, detail: &str) -> Result<()> {
        self.record_pack_event(pack_id, "exported", Some(detail))
    }

    /// List all packs with their path counts, ordered by name.
    pub fn list_packs(&self) -> Result<Vec<PackRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.description,
                    COUNT(pp.path) AS path_count, p.created_at,
                    (SELECT MAX(at) FROM pack_events WHERE pack_id = p.id) AS updated_at
             FROM packs p
             LEFT JOIN pack_paths pp ON pp.pack_id = p.id
             GROUP BY p.id
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PackRecord {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                path_count: r.get::<_, i64>(3)? as usize,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Look up a pack by name (case-insensitive). Returns None if not found.
    pub fn pack_by_name(&self, name: &str) -> Result<Option<PackRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.description,
                    COUNT(pp.path) AS path_count, p.created_at,
                    (SELECT MAX(at) FROM pack_events WHERE pack_id = p.id) AS updated_at
             FROM packs p
             LEFT JOIN pack_paths pp ON pp.pack_id = p.id
             WHERE lower(p.name) = lower(?1)
             GROUP BY p.id",
        )?;
        let mut rows = stmt.query_map(params![name], |r| {
            Ok(PackRecord {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                path_count: r.get::<_, i64>(3)? as usize,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// List all paths in a pack, ordered by path.
    pub fn pack_paths(&self, pack_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM pack_paths WHERE pack_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![pack_id], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// A pack's event history (4.1), chronological — the source for a changelog (OKF
    /// bundle's `log.md`, 4.2) or an `indexa pack show` history section.
    pub fn pack_events(&self, pack_id: &str) -> Result<Vec<PackEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT pack_id, event, detail, at FROM pack_events
              WHERE pack_id = ?1 ORDER BY at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![pack_id], |r| {
            Ok(PackEvent {
                pack_id: r.get(0)?,
                event: r.get(1)?,
                detail: r.get(2)?,
                at: r.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Delete a pack and all its path associations.
    pub fn delete_pack(&mut self, pack_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM packs WHERE id = ?1", params![pack_id])?;
        Ok(())
    }
}
