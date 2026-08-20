//! Note anchor writes and queries (the `note_anchors` table, 2.6).
//!
//! An `add_note` may optionally anchor itself to a code path or bare symbol name so graph
//! tools (`dependencies`/`blast_radius`/`symbol_context`) can surface "you already wrote
//! something about this" inline — the memory × graph intersection.

use super::Store;
use anyhow::Result;
use rusqlite::params;

/// One note anchored to a code path or symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteAnchor {
    pub note_path: String,
    pub anchor: String,
    pub anchor_kind: String,
    pub title: String,
    pub pack: String,
}

impl Store {
    /// Record (or replace) the anchor for `note_path`. A note has at most one anchor —
    /// re-submitting the same note (idempotent overwrite, per `write_note_file`'s own
    /// contract) replaces its anchor row too.
    pub fn upsert_note_anchor(
        &mut self,
        note_path: &str,
        anchor: &str,
        anchor_kind: &str,
        title: &str,
        pack: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO note_anchors (note_path, anchor, anchor_kind, title, pack)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(note_path) DO UPDATE SET
                anchor = excluded.anchor,
                anchor_kind = excluded.anchor_kind,
                title = excluded.title,
                pack = excluded.pack",
            params![note_path, anchor, anchor_kind, title, pack],
        )?;
        Ok(())
    }

    /// Every note anchored to `anchor` (a code path or bare symbol name), most recent
    /// `note_path` first (a proxy for recency — filenames embed no timestamp, but a later
    /// note on the same anchor sorts after lexically-earlier hash suffixes often enough to
    /// be a reasonable default; callers needing true recency should stat the file).
    pub fn note_anchors_for(&self, anchor: &str) -> Result<Vec<NoteAnchor>> {
        let mut stmt = self.conn.prepare(
            "SELECT note_path, anchor, anchor_kind, title, pack FROM note_anchors
              WHERE anchor = ?1 ORDER BY note_path DESC",
        )?;
        let rows = stmt.query_map(params![anchor], |r| {
            Ok(NoteAnchor {
                note_path: r.get(0)?,
                anchor: r.get(1)?,
                anchor_kind: r.get(2)?,
                title: r.get(3)?,
                pack: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}
