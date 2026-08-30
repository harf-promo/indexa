//! Context Packs: named, cross-directory context bundles.

use super::{PackEvent, PackItemRecord, PackRecord, Store};
use anyhow::{bail, Result};
use rusqlite::params;

/// Cap on a pinned snapshot's captured text (chars), generous but bounded so pinning a whole
/// directory can't balloon the DB with an unbounded blob. ~500k chars ≈ 125k tokens under the
/// query crate's `approx_tokens` 4-chars/token estimate — comfortably past any single AI tool's
/// context window, so truncation here is a safety valve, not a normal-case limit.
const PINNED_SNAPSHOT_CHAR_CAP: usize = 500_000;

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

    /// Record that a pack was refreshed (G2b) — `detail` is a short human-readable summary of
    /// how many stale members were reindexed vs. genuinely vanished from disk (e.g. "3
    /// reindexed, 1 vanished (left flagged)"). Shows up in `pack show` history and the OKF
    /// bundle's `log.md`, same as [`Store::record_pack_exported`].
    pub fn record_pack_refreshed(&mut self, pack_id: &str, detail: &str) -> Result<()> {
        self.record_pack_event(pack_id, "refreshed", Some(detail))
    }

    /// A pack's indexed member files whose stored chunks are out of date with the file on disk —
    /// the "stale" set behind `pack show`, the export headers' `stale_files` count, and `pack
    /// refresh`. Each member path (a file or a directory prefix) is expanded via
    /// [`super::entries::subtree_match`] to the *indexed* files at or under it (those carrying
    /// chunks); each is then stat'd on the LIVE disk and kept when it is no longer current per
    /// [`Store::chunks_current_for_mtime`] (missing/partial embeddings, or indexed before the
    /// file's current mtime). A member that can't be stat'd (deleted/unreadable) counts as stale
    /// too — it no longer matches what was indexed, and `pack refresh` leaves it flagged rather
    /// than silently dropping it (see that function's doc comment). Returned sorted (BTreeSet),
    /// deduped across overlapping members.
    ///
    /// This deliberately touches the disk, unusually for the store: `entries.modified_s`
    /// reflects only the last *scan*, so a file edited without a rescan would look fresh.
    /// `chunks_current_for_mtime` is built for exactly this caller-supplied-live-mtime check.
    ///
    /// Chunk-level only: a pack export built from summaries (the non-`--signatures` path) can
    /// still show a stale-in-spirit *summary* after this reports 0 stale, because summary
    /// freshness isn't part of this check — only the underlying chunk content is. `pack refresh`
    /// only reindexes chunks (`cmd_deep`); picking up a summary/description change needs a
    /// separate `indexa summarize <path>`, same as it always has.
    pub fn stale_pack_paths(&self, pack_id: &str) -> Result<Vec<String>> {
        use std::collections::BTreeSet;
        let members = self.pack_paths(pack_id)?;

        // Expand every member to the indexed files (those carrying chunks) at or under it.
        let mut indexed: BTreeSet<String> = BTreeSet::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT entry_path FROM chunks
                  WHERE entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\'",
            )?;
            for member in &members {
                let (exact, like) = super::entries::subtree_match(member);
                let rows = stmt.query_map(params![exact, like], |r| r.get::<_, String>(0))?;
                for p in rows {
                    indexed.insert(p?);
                }
            }
        }

        let mut stale = Vec::new();
        for path in indexed {
            let live_mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let current = match live_mtime {
                Some(m) => self.chunks_current_for_mtime(&path, m).unwrap_or(false),
                None => false, // gone/unreadable → no longer matches what was indexed
            };
            if !current {
                stale.push(path);
            }
        }
        Ok(stale)
    }

    /// A pack's members with their per-item inclusion mode (v0.78) — the render-time input for
    /// `pack export`'s XML/MD/JSON path: a `"reference"` item still walks the live summaries tree
    /// via [`super::export`]-style callers' own `build_tree`; a `"pinned"` item renders its
    /// `pinned_snapshot` verbatim instead. Ordered by path, like [`Store::pack_paths`].
    pub fn pack_item_records(&self, pack_id: &str) -> Result<Vec<PackItemRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, inclusion_mode, pinned_snapshot FROM pack_paths
              WHERE pack_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![pack_id], |r| {
            Ok(PackItemRecord {
                path: r.get(0)?,
                inclusion_mode: r.get(1)?,
                pinned_snapshot: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Concatenate the indexed (L2 raw chunk) content at or under `member_path` into one string —
    /// the snapshot a `"pinned"` pack item freezes. `None` when nothing is indexed there yet.
    /// Reuses the same subtree expansion [`Store::stale_pack_paths`] uses, so a pinned directory
    /// captures every indexed file beneath it, not just the directory's own (nonexistent) chunks.
    /// Capped at [`PINNED_SNAPSHOT_CHAR_CAP`] chars — a truncated capture is marked as such rather
    /// than silently cut off.
    fn capture_l2_snapshot(&self, member_path: &str) -> Result<Option<String>> {
        let (exact, like) = super::entries::subtree_match(member_path);
        let mut stmt = self.conn.prepare(
            "SELECT entry_path, seq, heading, text FROM chunks
              WHERE entry_path = ?1 OR entry_path LIKE ?2 ESCAPE '\\'
              ORDER BY entry_path, seq",
        )?;
        let rows = stmt.query_map(params![exact, like], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;

        let mut out = String::new();
        let mut truncated = false;
        for row in rows {
            let (path, seq, heading, text) = row?;
            if out.len() >= PINNED_SNAPSHOT_CHAR_CAP {
                truncated = true;
                break;
            }
            if heading.is_empty() {
                out.push_str(&format!("### {path} [{seq}]\n"));
            } else {
                out.push_str(&format!("### {path} [{seq}] {heading}\n"));
            }
            out.push_str(&text);
            out.push_str("\n\n");
        }
        if out.is_empty() {
            return Ok(None);
        }
        if truncated {
            out.push_str("…(pinned snapshot truncated at capture time)\n");
        }
        Ok(Some(out))
    }

    /// Set one pack member's inclusion mode (v0.78): `"reference"` (a live pointer, resolved
    /// fresh at export time — the default, and the ONLY behavior every pack item had before this
    /// field existed) or `"pinned"` (freezes the item's current L2 content into
    /// `pinned_snapshot`, captured right now via [`Store::capture_l2_snapshot`]).
    ///
    /// Switching back to `"reference"` clears `pinned_snapshot` (it would otherwise be a stale,
    /// invisible leftover — dead weight in the DB and a footgun if some future code path started
    /// reading it without checking `inclusion_mode` first). Re-pinning an already-pinned item
    /// re-captures the snapshot from the CURRENT chunks, so "pin" always means "freeze what's
    /// indexed right now", not "freeze once and never again unless you unpin first".
    ///
    /// Returns the number of rows changed (0 = no such pack/path). Errors on any `mode` other
    /// than `"reference"`/`"pinned"` — there is deliberately no CHECK constraint on the column
    /// (see the schema DDL's comment), so this is the one enforcement point.
    pub fn set_pack_item_inclusion_mode(
        &mut self,
        pack_id: &str,
        path: &str,
        mode: &str,
    ) -> Result<usize> {
        if mode != "reference" && mode != "pinned" {
            bail!("invalid inclusion mode '{mode}' — must be 'reference' or 'pinned'");
        }
        let snapshot = if mode == "pinned" {
            self.capture_l2_snapshot(path)?
        } else {
            None
        };
        let n = self.conn.execute(
            "UPDATE pack_paths SET inclusion_mode = ?1, pinned_snapshot = ?2
              WHERE pack_id = ?3 AND path = ?4",
            params![mode, snapshot, pack_id, path],
        )?;
        Ok(n)
    }
}
