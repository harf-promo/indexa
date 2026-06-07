//! Saved searches: named, reusable `ask` queries (the `saved_queries` table).

use super::Store;
use anyhow::Result;
use rusqlite::params;

/// A named, reusable query: a question plus the retrieval mode + optional scope to run it with.
#[derive(Debug, Clone)]
pub struct SavedQuery {
    pub name: String,
    pub question: String,
    /// Retrieval mode: `rrf` | `sparse` | `dense` | `agentic`.
    pub mode: String,
    pub scope: Option<String>,
    pub created_at: i64,
}

impl Store {
    /// Create or replace a saved query by name.
    pub fn save_query(
        &mut self,
        name: &str,
        question: &str,
        mode: &str,
        scope: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO saved_queries (name, question, mode, scope)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
                question = excluded.question,
                mode     = excluded.mode,
                scope    = excluded.scope,
                created_at = unixepoch()",
            params![name, question, mode, scope],
        )?;
        Ok(())
    }

    /// Look up a saved query by name (case-sensitive).
    pub fn get_saved_query(&self, name: &str) -> Result<Option<SavedQuery>> {
        let row = self
            .conn
            .query_row(
                "SELECT name, question, mode, scope, created_at FROM saved_queries WHERE name = ?1",
                params![name],
                |r| {
                    Ok(SavedQuery {
                        name: r.get(0)?,
                        question: r.get(1)?,
                        mode: r.get(2)?,
                        scope: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .ok();
        Ok(row)
    }

    /// All saved queries, alphabetically by name.
    pub fn list_saved_queries(&self) -> Result<Vec<SavedQuery>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, question, mode, scope, created_at FROM saved_queries ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SavedQuery {
                name: r.get(0)?,
                question: r.get(1)?,
                mode: r.get(2)?,
                scope: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Delete a saved query. Returns the number of rows removed (0 = no such name).
    pub fn delete_saved_query(&mut self, name: &str) -> Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM saved_queries WHERE name = ?1", params![name])?;
        Ok(n)
    }
}
