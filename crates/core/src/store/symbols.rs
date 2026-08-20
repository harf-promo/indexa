//! Code symbol writes and queries (the `symbols` table, 2.1).
//!
//! A symbol row is richer than a bare `defines` edge (name only): it carries the symbol's
//! kind (`"fn"`, `"struct"`, `"class"`, …) and 1-based inclusive line range, extracted by
//! `indexa_parsers::code::extract_defines` alongside the existing edges. Backs diff→symbol
//! mapping (`changed_impact`) and kind-aware graph-tool output.

use super::{Store, SymbolRecord};
use anyhow::Result;
use rusqlite::params;

impl Store {
    /// Replace every symbol row for each distinct `path` in `symbols` — same
    /// delete-by-path-then-insert contract as [`Store::upsert_edges`], so a re-`deep` of a
    /// file replaces its symbol rows rather than accumulating stale ones.
    pub fn upsert_symbols(&mut self, symbols: &[SymbolRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut del = tx.prepare_cached("DELETE FROM symbols WHERE path = ?1")?;
            let mut cleared: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for s in symbols {
                if cleared.insert(s.path.as_str()) {
                    del.execute(params![s.path])?;
                }
            }
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO symbols (path, name, kind, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for s in symbols {
                ins.execute(params![s.path, s.name, s.kind, s.start_line, s.end_line])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every symbol defined in `path`, ordered by start line — a file's outline.
    pub fn symbols_in_file(&self, path: &str) -> Result<Vec<SymbolRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, name, kind, start_line, end_line FROM symbols
              WHERE path = ?1 ORDER BY start_line",
        )?;
        let rows = stmt.query_map(params![path], |r| {
            Ok(SymbolRecord {
                path: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                start_line: r.get(3)?,
                end_line: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Symbols in `path` whose line range overlaps `start_line..=end_line` — the diff→symbol
    /// mapping `changed_impact` (2.3) needs: which symbols does a changed hunk touch?
    /// Overlap test: `symbol.start_line <= hunk.end_line AND symbol.end_line >=
    /// hunk.start_line`.
    pub fn symbols_overlapping(
        &self,
        path: &str,
        start_line: i64,
        end_line: i64,
    ) -> Result<Vec<SymbolRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, name, kind, start_line, end_line FROM symbols
              WHERE path = ?1 AND start_line <= ?3 AND end_line >= ?2
              ORDER BY start_line",
        )?;
        let rows = stmt.query_map(params![path, start_line, end_line], |r| {
            Ok(SymbolRecord {
                path: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                start_line: r.get(3)?,
                end_line: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}
