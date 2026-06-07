//! Context Packs: named, cross-directory context bundles.

use super::{PackRecord, Store};
use anyhow::Result;
use rusqlite::params;

impl Store {
    /// Create a new pack with a unique name. Returns the generated pack ID.
    pub fn create_pack(&mut self, name: &str, description: Option<&str>) -> Result<String> {
        let id: String = self.conn.query_row(
            "INSERT INTO packs (id, name, description)
             VALUES (lower(hex(randomblob(8))), ?1, ?2)
             RETURNING id",
            params![name, description],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Rename a pack. Errors if `new_name` is already taken (UNIQUE name constraint).
    /// Returns the number of rows changed (0 = no pack with that id).
    pub fn rename_pack(&mut self, pack_id: &str, new_name: &str) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE packs SET name = ?1 WHERE id = ?2",
            params![new_name, pack_id],
        )?;
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
        tx.commit()?;
        Ok(())
    }

    /// List all packs with their path counts, ordered by name.
    pub fn list_packs(&self) -> Result<Vec<PackRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.description,
                    COUNT(pp.path) AS path_count, p.created_at
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
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Look up a pack by name (case-insensitive). Returns None if not found.
    pub fn pack_by_name(&self, name: &str) -> Result<Option<PackRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.description,
                    COUNT(pp.path) AS path_count, p.created_at
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

    /// Delete a pack and all its path associations.
    pub fn delete_pack(&mut self, pack_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM packs WHERE id = ?1", params![pack_id])?;
        Ok(())
    }
}
