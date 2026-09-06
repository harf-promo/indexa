//! Typed durable memory storage — see [`crate::memory`] for the domain types and for why this
//! is a separate table from the Decision Ledger rather than a new `decision_type`.
//!
//! # `verify_cmd` is stored and printed, never executed
//!
//! A memory may carry the command that re-checks its claim (`cargo test -p indexa-core
//! store::memories`, say). This layer treats that string as **data**. Nothing here, and nothing
//! on the MCP surface, runs it. Only `indexa memory verify --run` executes it, from the CLI,
//! after a human asked for it by name.
//!
//! That is not caution for its own sake. Memories are portable — the plan is for them to travel
//! in Context Packs — so a memory arriving from someone else's export is untrusted input. An
//! index that shelled out stored strings on read would turn "import a colleague's pack" into
//! remote code execution.
//!
//! # Memories are not orphan-pruned
//!
//! `prune.rs`'s `orphan_rows_for` deliberately does not include this table. A note explaining
//! why a file was removed is at its most valuable precisely when the file is gone; cascading
//! the delete would destroy the record at the moment it starts mattering.

use super::Store;
use crate::memory::{Author, MemoryKind, MemoryRecord, MemoryStatus, NewMemory, VerifyStatus};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// Column list shared by every read, so the `row_to_memory` indices can't drift per query.
const COLS: &str = "id, kind, text, text_sha256, subject, confidence, author, source_path, \
                    source_sha256, patch_id, source_decision_id, verify_cmd, verify_status, \
                    verified_at, tags, status, parent_id, superseded_by, valid_from, valid_to, \
                    created_at, updated_at";

/// [`COLS`] with every column qualified by `alias`, for the joined FTS read. Derived from the
/// same constant so a new column can never be added to one read and forgotten in the other.
fn qualified_cols(alias: &str) -> String {
    COLS.split(',')
        .map(|c| format!("{alias}.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One Decision Ledger annotation, in the shape the memory bridge needs it.
#[derive(Debug, Clone)]
pub struct AdoptableAnnotation {
    pub decision_id: i64,
    pub subject: String,
    /// The annotation's answer text — its `chosen` value.
    pub text: String,
    pub patch_id: Option<String>,
}

/// A row's counts, for `indexa memory list` headers and `get_stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryCounts {
    pub active: u64,
    pub retired: u64,
    pub verified: u64,
    pub unverified: u64,
    pub failed: u64,
    pub aged: u64,
}

fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    let kind: String = r.get(1)?;
    let author: String = r.get(6)?;
    let verify_status: String = r.get(12)?;
    let tags: String = r.get(14)?;
    let status: String = r.get(15)?;
    Ok(MemoryRecord {
        id: r.get(0)?,
        // An unparseable enum is a corrupted or hand-edited row, not a reason to fail the whole
        // read: fall back to the least-trusted / most-conservative value so a bad row can never
        // masquerade as a verified observation.
        kind: MemoryKind::parse(&kind).unwrap_or(MemoryKind::Hypothesis),
        text: r.get(2)?,
        text_sha256: r.get(3)?,
        subject: r.get(4)?,
        confidence: r.get::<_, f64>(5)? as f32,
        author: Author::parse(&author).unwrap_or(Author::Agent),
        source_path: r.get(7)?,
        source_sha256: r.get(8)?,
        patch_id: r.get(9)?,
        source_decision_id: r.get(10)?,
        verify_cmd: r.get(11)?,
        verify_status: VerifyStatus::parse(&verify_status).unwrap_or(VerifyStatus::Unverified),
        verified_at: r.get(13)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        status: MemoryStatus::parse(&status).unwrap_or(MemoryStatus::Active),
        parent_id: r.get(16)?,
        superseded_by: r.get(17)?,
        valid_from: r.get(18)?,
        valid_to: r.get(19)?,
        created_at: r.get(20)?,
        updated_at: r.get(21)?,
        // Filled by `hydrate_paths`; a bare row read leaves it empty rather than issuing an
        // N+1 query per row.
        paths: Vec::new(),
    })
}

impl Store {
    /// Record a memory, returning `(id, confidence_was_clamped)`.
    ///
    /// Idempotent on the claim text: re-recording an identical `text` while an active row
    /// already holds it returns that row's id and writes nothing. That is what makes it safe
    /// for an agent to `remember` the same thing across sessions without the store filling with
    /// near-identical rows.
    ///
    /// `confidence` is clamped to the author's ceiling (0.75 for an agent). The returned flag
    /// says whether that happened, so a surface can tell the caller its number was reduced
    /// rather than quietly lowering it.
    pub fn record_memory(&mut self, m: &NewMemory) -> Result<(i64, bool)> {
        let text_sha = super::hex_digest(<sha2::Sha256 as sha2::Digest>::digest(m.text.as_bytes()));
        if let Some(existing) = self.active_memory_id_by_hash(&text_sha)? {
            return Ok((existing, false));
        }
        let (confidence, clamped) = m.effective_confidence();
        let tags = serde_json::to_string(&m.tags)?;

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO memories
                 (kind, text, text_sha256, subject, confidence, author, source_path,
                  source_sha256, patch_id, source_decision_id, verify_cmd, tags, valid_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     COALESCE(?13, unixepoch()))",
            params![
                m.kind.as_str(),
                m.text,
                text_sha,
                m.subject,
                confidence as f64,
                m.author.as_str(),
                m.source_path,
                m.source_sha256,
                m.patch_id,
                m.source_decision_id,
                m.verify_cmd,
                tags,
                m.valid_from,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO memories_fts (text, tags, memory_id) VALUES (?1, ?2, ?3)",
            params![m.text, m.tags.join(" "), id],
        )?;
        for p in &m.paths {
            tx.execute(
                "INSERT INTO memory_paths (memory_id, path) VALUES (?1, ?2)
                 ON CONFLICT DO NOTHING",
                params![id, p],
            )?;
        }
        tx.commit()?;
        Ok((id, clamped))
    }

    fn active_memory_id_by_hash(&self, hash: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM memories WHERE text_sha256 = ?1 AND status = 'active'",
                params![hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Read one memory by id, with its `memory_paths` hydrated.
    pub fn memory_by_id(&self, id: i64) -> Result<Option<MemoryRecord>> {
        let found = self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM memories WHERE id = ?1"),
                params![id],
                row_to_memory,
            )
            .optional()?;
        match found {
            Some(mut m) => {
                m.paths = self.memory_paths_for(id)?;
                Ok(Some(m))
            }
            None => Ok(None),
        }
    }

    fn memory_paths_for(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM memory_paths WHERE memory_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Active, retrievable memories, most-trusted first.
    ///
    /// Ordering is `kind` trust rank, then confidence, then recency of verification — the same
    /// order the retrieval block renders in, so a caller reading a truncated list is reading
    /// the most trustworthy part of it. Rows whose validity window has closed, and rows whose
    /// verification failed, are excluded.
    pub fn active_memories(
        &self,
        kinds: Option<&[MemoryKind]>,
        min_confidence: f32,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM memories
              WHERE status = 'active'
                AND verify_status != 'failed'
                AND confidence >= ?1
                AND (valid_to IS NULL OR valid_to > unixepoch())
              ORDER BY confidence DESC, COALESCE(verified_at, 0) DESC, id DESC
              LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![min_confidence as f64, limit as i64], row_to_memory)?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        if let Some(want) = kinds {
            out.retain(|m| want.contains(&m.kind));
        }
        // Trust rank is a Rust-side property of the enum, not a column, so the final ordering
        // is applied here rather than in SQL. Stable sort keeps the SQL tiebreak intact.
        out.sort_by_key(|m| std::cmp::Reverse(m.kind.trust_rank()));
        Ok(out)
    }

    /// Memories relevant to any of `paths` — matched on `subject` or on a `memory_paths` row.
    pub fn memories_for_paths(&self, paths: &[String], limit: usize) -> Result<Vec<MemoryRecord>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; paths.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM memories m
              WHERE m.status = 'active'
                AND m.verify_status != 'failed'
                AND (m.valid_to IS NULL OR m.valid_to > unixepoch())
                AND (m.subject IN ({placeholders})
                     OR EXISTS (SELECT 1 FROM memory_paths mp
                                 WHERE mp.memory_id = m.id
                                   AND mp.path IN ({placeholders})))
              ORDER BY m.confidence DESC, m.id DESC
              LIMIT {limit}"
        ))?;
        // The path list is bound twice (subject IN …, and memory_paths IN …).
        let doubled: Vec<&String> = paths.iter().chain(paths.iter()).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(doubled), row_to_memory)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Full-text search over memory claims and tags.
    pub fn search_memories(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        let fts = super::search::build_fts_query(query);
        if fts.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM memories_fts f
               JOIN memories m ON m.id = CAST(f.memory_id AS INTEGER)
              WHERE memories_fts MATCH ?1
                AND m.status = 'active'
                AND m.verify_status != 'failed'
              ORDER BY bm25(memories_fts), m.confidence DESC
              LIMIT ?2",
            qualified_cols("m")
        ))?;
        let rows = stmt.query_map(params![fts, limit as i64], row_to_memory)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Record `replacement` as the successor of `old_id`, chaining both directions.
    ///
    /// The old row stays: it is retired, not deleted, and keeps pointing at what replaced it, so
    /// the history of a changing belief is readable rather than overwritten.
    pub fn supersede_memory(
        &mut self,
        old_id: i64,
        replacement: &NewMemory,
    ) -> Result<(i64, bool)> {
        let (new_id, clamped) = self.record_memory(replacement)?;
        if new_id == old_id {
            // The replacement text is identical to the row being superseded — nothing changed,
            // so chaining it to itself would create a cycle.
            return Ok((new_id, clamped));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE memories SET superseded_by = ?1, status = 'retired',
                                 updated_at = unixepoch()
              WHERE id = ?2",
            params![new_id, old_id],
        )?;
        tx.execute(
            "UPDATE memories SET parent_id = ?1, updated_at = unixepoch() WHERE id = ?2",
            params![old_id, new_id],
        )?;
        tx.commit()?;
        Ok((new_id, clamped))
    }

    /// Set a memory's verification status. Returns whether a row was affected.
    pub fn set_memory_verify_status(
        &mut self,
        id: i64,
        status: VerifyStatus,
        at: Option<i64>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE memories
                SET verify_status = ?1,
                    verified_at = CASE WHEN ?1 IN ('verified','failed')
                                       THEN COALESCE(?2, unixepoch()) ELSE verified_at END,
                    updated_at = unixepoch()
              WHERE id = ?3",
            params![status.as_str(), at, id],
        )?;
        Ok(n > 0)
    }

    /// Retire a memory: keep the row, stop retrieving it, free its text hash for re-learning.
    pub fn retire_memory(&mut self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE memories SET status = 'retired', updated_at = unixepoch() WHERE id = ?1",
            params![id],
        )?;
        Ok(n > 0)
    }

    /// Close a memory's validity window — the claim was true, and now isn't.
    pub fn set_memory_valid_to(&mut self, id: i64, valid_to: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE memories SET valid_to = ?1, updated_at = unixepoch() WHERE id = ?2",
            params![valid_to, id],
        )?;
        Ok(n > 0)
    }

    /// Decision Ledger `annotation` rows, for the one-shot `indexa memory adopt-annotations`
    /// bridge.
    ///
    /// Lives here rather than in `decisions.rs` because it exists solely for the memory
    /// layer's benefit — the ledger has no use for it, and the ledger rows are read-only to
    /// this module. An annotation's answer text is its `chosen` value; rows without one are
    /// skipped rather than adopted as an empty claim.
    pub fn annotation_decisions(&self) -> Result<Vec<AdoptableAnnotation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, COALESCE(chosen, ''), patch_id
               FROM decisions
              WHERE decision_type = 'annotation' AND status = 'decided'
              ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AdoptableAnnotation {
                decision_id: r.get(0)?,
                subject: r.get(1)?,
                text: r.get(2)?,
                patch_id: r.get(3)?,
            })
        })?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|a| !a.text.trim().is_empty())
            .collect())
    }

    pub fn memory_counts(&self) -> Result<MemoryCounts> {
        let mut c = MemoryCounts::default();
        let mut stmt = self
            .conn
            .prepare("SELECT status, verify_status, COUNT(*) FROM memories GROUP BY 1, 2")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        for row in rows {
            let (status, verify, n) = row?;
            match status.as_str() {
                "active" => c.active += n,
                _ => c.retired += n,
            }
            match verify.as_str() {
                "verified" => c.verified += n,
                "failed" => c.failed += n,
                "aged" => c.aged += n,
                _ => c.unverified += n,
            }
        }
        Ok(c)
    }

    /// Conservatively age unverified inference and hypothesis rows. Returns how many changed.
    ///
    /// The contract, which the tests pin:
    /// - **Only** `inferred` and `hypothesis`, and **only** while `verify_status='unverified'`.
    ///   An observation or a trusted statement does not become less true because time passed.
    /// - Confidence is multiplied by `factor` and floored, never zeroed.
    /// - **Nothing is ever deleted.** The row is marked `aged` and stays readable.
    /// - Nothing calls this on a schedule. It runs when an operator runs it.
    pub fn age_unverified_memories(&mut self, older_than_secs: i64, factor: f32) -> Result<usize> {
        /// Aged rows keep a floor so a long-lived index can't decay a claim to a number that
        /// reads as "definitely false" when what is actually meant is "nobody ever checked".
        const AGED_CONFIDENCE_FLOOR: f64 = 0.05;
        let factor = factor.clamp(0.0, 1.0) as f64;
        let decayable: Vec<&str> = MemoryKind::ALL
            .iter()
            .filter(|k| k.decayable())
            .map(|k| k.as_str())
            .collect();
        let placeholders = vec!["?"; decayable.len()].join(",");
        // Every placeholder is unnumbered and bound strictly in source order — factor, floor,
        // each decayable kind, then the age cutoff. Mixing `?N` with a variable-length `IN (?)`
        // list is where this kind of query goes wrong, so it is avoided outright.
        let sql = format!(
            "UPDATE memories
                SET confidence = MAX(confidence * ?, ?),
                    verify_status = 'aged',
                    updated_at = unixepoch()
              WHERE status = 'active'
                AND verify_status = 'unverified'
                AND kind IN ({placeholders})
                AND created_at <= unixepoch() - ?"
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(factor), Box::new(AGED_CONFIDENCE_FLOOR)];
        for k in &decayable {
            args.push(Box::new(k.to_string()));
        }
        args.push(Box::new(older_than_secs));
        let n = self.conn.execute(
            &sql,
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
        )?;
        Ok(n)
    }
}
