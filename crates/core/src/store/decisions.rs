//! Decision Ledger (v0.22): reads/writes for the `decisions` + `decision_paths` tables.
//!
//! One row = one question + its answer. A row fills in place exactly once
//! (open → decided/dismissed/expired); changing or re-asking APPENDS a new row
//! chained via `parent_id`, and answering the new row stamps the prior row's
//! `superseded_by` — the only post-decision mutation. Current state is therefore
//! `status='decided' AND superseded_by IS NULL`. Downstream domain tables
//! (classifications, weights, summary_queue) stay authoritative for runtime and
//! are idempotent projections of the latest decided revision.
//!
//! **Decisions persist across entry removal by design** (like `importance_weights`):
//! a recorded answer is standing user intent that can outlive its `entries` row.
//! The entries.rs delete paths do NOT touch these tables; vanished subjects are
//! expired by the sweep via [`Store::expire_decision`] — recorded, never dropped.

use super::search::like_prefix;
use super::types::{DecisionRecord, NewDecision};
use super::Store;
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Transaction};

/// Shared SELECT column list — keep in sync with [`row_to_decision`].
const DECISION_COLS: &str = "id, decision_type, subject, params, options, auto_value, chosen, \
     source, confidence, evidence_hash, priority, status, parent_id, superseded_by, \
     effects, effects_applied_at, created_at, decided_at";

fn row_to_decision(r: &rusqlite::Row) -> rusqlite::Result<DecisionRecord> {
    Ok(DecisionRecord {
        id: r.get(0)?,
        decision_type: r.get(1)?,
        subject: r.get(2)?,
        params: r.get(3)?,
        options: r.get(4)?,
        auto_value: r.get(5)?,
        chosen: r.get(6)?,
        source: r.get(7)?,
        // Stored as REAL; read as f64 then narrow (same as classifications.confidence).
        confidence: r.get::<_, Option<f64>>(8)?.map(|c| c as f32),
        evidence_hash: r.get(9)?,
        priority: r.get(10)?,
        status: r.get(11)?,
        parent_id: r.get(12)?,
        superseded_by: r.get(13)?,
        effects: r.get(14)?,
        effects_applied_at: r.get(15)?,
        created_at: r.get(16)?,
        decided_at: r.get(17)?,
    })
}

/// Mark an OPEN row decided and, in the same transaction, stamp its parent's
/// `superseded_by` (the revision-chain link is created at answer time so the
/// prior answer stays "latest decided" until the re-ask is actually resolved).
/// The caller has already verified the row is open.
fn decide_row(tx: &Transaction, id: i64, chosen: &str, source: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE decisions
            SET chosen = ?2, source = ?3, status = 'decided', decided_at = unixepoch()
          WHERE id = ?1",
        params![id, chosen, source],
    )?;
    let parent_id: Option<i64> = tx.query_row(
        "SELECT parent_id FROM decisions WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    if let Some(pid) = parent_id {
        // The IS NULL guard makes single-stamp a local invariant rather than an
        // emergent one (parents are always the latest_decided head today, but
        // nothing here should rely on that).
        tx.execute(
            "UPDATE decisions SET superseded_by = ?2
              WHERE id = ?1 AND superseded_by IS NULL",
            params![pid, id],
        )?;
    }
    Ok(())
}

impl Store {
    // ── Recording questions ───────────────────────────────────────────────────

    /// Record a new open question. Returns `None` (without inserting) when:
    /// - an open row for `(decision_type, subject)` already exists — racing
    ///   detectors resolve via `ON CONFLICT DO NOTHING` on the partial unique
    ///   index, or
    /// - a dismissed row for the same key carries the same `evidence_hash` —
    ///   sticky dismissal: a dismissed question only returns when the evidence
    ///   behind it changes.
    pub fn record_decision(&mut self, d: NewDecision) -> Result<Option<i64>> {
        self.record_decision_inner(&d, None)
    }

    /// Re-ask: record a new revision of `prior_id`'s question (`parent_id` wired
    /// in). The prior row's `superseded_by` is NOT stamped here — that happens
    /// when the new row is answered, so the prior answer stays authoritative
    /// until the user actually resolves the re-ask. Same dedup rules as
    /// [`Store::record_decision`].
    pub fn supersede_with(&mut self, prior_id: i64, d: NewDecision) -> Result<Option<i64>> {
        self.record_decision_inner(&d, Some(prior_id))
    }

    fn record_decision_inner(
        &mut self,
        d: &NewDecision,
        parent_id: Option<i64>,
    ) -> Result<Option<i64>> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let dismissed_same_evidence: bool = tx
            .prepare(
                "SELECT 1 FROM decisions
                  WHERE decision_type = ?1 AND subject = ?2
                    AND status = 'dismissed' AND evidence_hash = ?3",
            )?
            .exists(params![d.decision_type, d.subject, d.evidence_hash])?;
        if dismissed_same_evidence {
            return Ok(None);
        }
        let inserted = tx.execute(
            "INSERT INTO decisions
                 (decision_type, subject, params, options, auto_value, confidence,
                  evidence_hash, priority, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(decision_type, subject) WHERE status='open' DO NOTHING",
            params![
                d.decision_type,
                d.subject,
                d.params.to_string(),
                d.options.to_string(),
                d.auto_value,
                d.confidence.map(|c| c as f64),
                d.evidence_hash,
                d.priority,
                parent_id,
            ],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let id = tx.last_insert_rowid();
        {
            // OR IGNORE: callers may pass the subject both as subject and in `paths`.
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO decision_paths (decision_id, path) VALUES (?1, ?2)",
            )?;
            for p in &d.paths {
                stmt.execute(params![id, p])?;
            }
        }
        tx.commit()?;
        Ok(Some(id))
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    /// Open questions in inbox order (priority DESC, newest first; id breaks
    /// same-second ties), optionally filtered by decision type.
    pub fn open_decisions(
        &self,
        type_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DecisionRecord>> {
        let order = "ORDER BY priority DESC, created_at DESC, id DESC LIMIT ?";
        if let Some(t) = type_filter {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {DECISION_COLS} FROM decisions
                  WHERE status = 'open' AND decision_type = ?1 {order}2"
            ))?;
            let rows = stmt.query_map(params![t, limit as i64], row_to_decision)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        } else {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {DECISION_COLS} FROM decisions WHERE status = 'open' {order}1"
            ))?;
            let rows = stmt.query_map(params![limit as i64], row_to_decision)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }
    }

    /// Count of open questions (the inbox badge).
    pub fn open_decision_count(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE status = 'open'",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Look up one decision by id.
    pub fn decision_by_id(&self, id: i64) -> Result<Option<DecisionRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {DECISION_COLS} FROM decisions WHERE id = ?1"),
                params![id],
                row_to_decision,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The current answer for a key: the decided, un-superseded revision.
    /// Newest by id if several qualify (the answer-time `superseded_by` stamp
    /// makes more than one impossible in practice; ordering is defensive).
    pub fn latest_decided(
        &self,
        decision_type: &str,
        subject: &str,
    ) -> Result<Option<DecisionRecord>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {DECISION_COLS} FROM decisions
                      WHERE decision_type = ?1 AND subject = ?2
                        AND status = 'decided' AND superseded_by IS NULL
                      ORDER BY id DESC LIMIT 1"
                ),
                params![decision_type, subject],
                row_to_decision,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every revision recorded for a key, oldest first — the revision chain is
    /// readable inline because rows are append-only per revision.
    pub fn decision_history(
        &self,
        decision_type: &str,
        subject: &str,
    ) -> Result<Vec<DecisionRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DECISION_COLS} FROM decisions
              WHERE decision_type = ?1 AND subject = ?2 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![decision_type, subject], row_to_decision)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Does an `entries` row exist for `path`? Used by the expiry sweep to spot
    /// open questions whose subject vanished from the index.
    pub fn entry_exists(&self, path: &str) -> Result<bool> {
        self.conn
            .prepare_cached("SELECT 1 FROM entries WHERE path = ?1")?
            .exists(params![path])
            .map_err(Into::into)
    }

    /// Days since `path`'s entry was last modified — evidence for archive
    /// questions. `None` when the entry is missing OR has no recorded mtime
    /// (NULL mtime is "unknown", not evidence of age, so the archive detector
    /// and pre-dismissal both refuse to fingerprint it).
    pub fn entry_age_days(&self, path: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT (unixepoch() - modified_s) / 86400 FROM entries
                  WHERE path = ?1 AND modified_s IS NOT NULL",
                params![path],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// File count in the subtree rooted at `dir` — the cheap half of the
    /// archive evidence fingerprint. Subtree-exact via `subtree_match`, so a
    /// sibling sharing the string prefix (`/proj` vs `/projector`) never counts.
    pub fn count_files_under(&self, dir: &str) -> Result<i64> {
        let (exact, child) = super::entries::subtree_match(dir);
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM entries
                  WHERE kind = 'file' AND (path = ?1 OR path LIKE ?2 ESCAPE '\\')",
                params![exact, child],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Ids of decisions whose `decision_paths` include `path` (exact match) and
    /// that are still live: open, or decided and un-superseded. Lets detectors
    /// skip raising a question a standing decision already covers.
    pub fn decisions_touching_path(&self, path: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT dp.decision_id
               FROM decision_paths dp
               JOIN decisions d ON d.id = dp.decision_id
              WHERE dp.path = ?1
                AND (d.status = 'open'
                     OR (d.status = 'decided' AND d.superseded_by IS NULL))
              ORDER BY dp.decision_id",
        )?;
        let rows = stmt.query_map(params![path], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Called symbols defined in MORE than one file — the D2 bare-name
    /// ambiguity set — as `(symbol, caller_count)`, most-called first.
    /// One GROUP BY over `edges`; the definer paths are fetched per symbol via
    /// `edges_to("defines", …)`. Lives here (a ledger concern), not in
    /// edges.rs, on purpose: the graph surfaces consult the *answers* later.
    pub fn ambiguous_called_symbols(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT to_ref,
                    COUNT(DISTINCT CASE WHEN kind = 'calls' THEN from_path END) AS callers
               FROM edges
              WHERE kind IN ('calls', 'defines')
              GROUP BY to_ref
             HAVING COUNT(DISTINCT CASE WHEN kind = 'defines' THEN from_path END) > 1
                AND COUNT(DISTINCT CASE WHEN kind = 'calls' THEN from_path END) > 0
              ORDER BY callers DESC, to_ref
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Lifecycle transitions (open → decided/dismissed/expired) ─────────────

    /// Answer an open question. Errors when the row is missing or not open
    /// (a decided row is immutable — append a revision via
    /// [`Store::supersede_with`] instead). Stamps the parent's `superseded_by`
    /// in the same transaction when this row is a re-ask.
    pub fn answer_decision(&mut self, id: i64, chosen: &str, source: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match decision_status(&tx, id)? {
            None => bail!("no decision with id {id}"),
            Some(s) if s != "open" => bail!("decision {id} is '{s}', not open"),
            Some(_) => {}
        }
        decide_row(&tx, id, chosen, source)?;
        tx.commit()?;
        Ok(())
    }

    /// Answer every OPEN question of `decision_type` whose subject starts with
    /// `dir_prefix` (batch answer, e.g. `review answer --under ~/Downloads`).
    /// Returns the answered ids.
    pub fn answer_decisions_under(
        &mut self,
        dir_prefix: &str,
        decision_type: &str,
        chosen: &str,
        source: &str,
    ) -> Result<Vec<i64>> {
        let pattern = like_prefix(dir_prefix);
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM decisions
                  WHERE status = 'open' AND decision_type = ?1
                    AND subject LIKE ?2 ESCAPE '\\'
                  ORDER BY id",
            )?;
            let rows = stmt.query_map(params![decision_type, pattern], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for &id in &ids {
            decide_row(&tx, id, chosen, source)?;
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Dismiss an open question ("stop asking"). Sticky: the same question only
    /// returns when its `evidence_hash` changes — see [`Store::record_decision`].
    pub fn dismiss_decision(&mut self, id: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE decisions
                SET status = 'dismissed', source = 'system', decided_at = unixepoch()
              WHERE id = ?1 AND status = 'open'",
            params![id],
        )?;
        if n == 0 {
            bail!("decision {id} is not an open question");
        }
        Ok(())
    }

    /// Expire an open question whose subject vanished (sweep path). The note is
    /// recorded under the `expired` key in `params` so the history explains
    /// itself — recorded, never silently dropped.
    pub fn expire_decision(&mut self, id: i64, note: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT status, params FROM decisions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (status, params_text) = match row {
            None => bail!("no decision with id {id}"),
            Some(r) => r,
        };
        if status != "open" {
            bail!("decision {id} is '{status}', not open");
        }
        // Tolerate malformed params (hand-edited DB): start over from {}.
        let mut v: serde_json::Value =
            serde_json::from_str(&params_text).unwrap_or_else(|_| serde_json::json!({}));
        if !v.is_object() {
            v = serde_json::json!({});
        }
        v["expired"] = serde_json::Value::String(note.to_owned());
        tx.execute(
            "UPDATE decisions
                SET status = 'expired', source = 'system',
                    decided_at = unixepoch(), params = ?2
              WHERE id = ?1",
            params![id, v.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── Effects (crash-safe projection bookkeeping) ───────────────────────────

    /// Stamp what the projection actually did. A decided row commits BEFORE its
    /// projection runs; this stamp is the projection's receipt, and a decided
    /// row without it is a crash-repair target — see [`Store::unapplied_decided`].
    pub fn mark_effects_applied(
        &mut self,
        id: i64,
        effects_json: &serde_json::Value,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE decisions
                SET effects = ?2, effects_applied_at = unixepoch()
              WHERE id = ?1",
            params![id, effects_json.to_string()],
        )?;
        if n == 0 {
            bail!("no decision with id {id}");
        }
        Ok(())
    }

    /// Decided rows whose projection never stamped its receipt — the repair
    /// sweep re-runs these (projections are idempotent, so re-running is safe).
    ///
    /// Superseded rows are excluded even when unstamped: a newer revision's
    /// projection already expresses the current answer, and re-applying the
    /// stale one would overwrite it (crash before P's projection → re-ask C
    /// answered → repairing P would resurrect P's answer over C's).
    pub fn unapplied_decided(&self, limit: usize) -> Result<Vec<DecisionRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DECISION_COLS} FROM decisions
              WHERE status = 'decided' AND effects_applied_at IS NULL
                AND superseded_by IS NULL
              ORDER BY id LIMIT ?1"
        ))?;
        let rows = stmt.query_map(params![limit as i64], row_to_decision)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── One-time backfill ─────────────────────────────────────────────────────

    /// Import pre-ledger classification answers (`classifications.source IN
    /// ('user','ignored')`) as decided ledger rows, so the re-ask detector has a
    /// prior to compare against. Guarded on the ledger holding NO classification
    /// rows at all — runs exactly once per database, idempotent thereafter.
    ///
    /// `evidence_hash` is left `''`: no fingerprint existed when the user
    /// originally answered, and `''` means "re-askable on the first
    /// contradiction". The effects receipt is stamped immediately — the
    /// classifications table already reflects these answers, and re-projecting
    /// would pointlessly rewrite `confirmed_at`.
    pub fn backfill_classification_decisions(&mut self) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let has_any: bool = tx
            .prepare("SELECT 1 FROM decisions WHERE decision_type = 'classification' LIMIT 1")?
            .exists([])?;
        if has_any {
            return Ok(0);
        }
        let rows: Vec<(String, String, String, Option<i64>, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT path, category, source, confirmed_at, created_at
                   FROM classifications WHERE source IN ('user','ignored')",
            )?;
            let mapped = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let n = rows.len();
        for (path, category, source, confirmed_at, created_at) in rows {
            let chosen = if source == "ignored" {
                "ignore"
            } else {
                category.as_str()
            };
            let when = confirmed_at.unwrap_or(created_at);
            let effects = serde_json::json!({ "classification": chosen });
            tx.execute(
                "INSERT INTO decisions
                     (decision_type, subject, params, options, chosen, source, status,
                      evidence_hash, effects, effects_applied_at, created_at, decided_at)
                 VALUES ('classification', ?1, '{}', '[]', ?2, 'user', 'decided',
                         '', ?3, unixepoch(), ?4, ?4)",
                params![path, chosen, effects.to_string(), when],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT OR IGNORE INTO decision_paths (decision_id, path) VALUES (?1, ?2)",
                params![id, path],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    // ── Garbage collection ────────────────────────────────────────────────────

    /// Delete expired/dismissed rows resolved more than `older_than_secs` ago.
    /// Returns the number of rows removed (`decision_paths` rows cascade).
    ///
    /// Deliberately conservative (Phase 1): rows referenced by ANY other row's
    /// `parent_id` or `superseded_by` are kept, so a revision chain is never
    /// broken mid-walk — and decided-superseded history is not GC'd at all yet
    /// (the whole-chain-older-than-horizon walk is deferred until the ledger
    /// carries enough volume to need it). Open and decided-current rows are
    /// never candidates. Note that GC of a dismissed row also forgets its
    /// sticky dismissal: past the horizon the question may be asked again.
    pub fn gc_decisions(&mut self, older_than_secs: i64) -> Result<usize> {
        let n = self.conn.execute(
            &format!("DELETE FROM decisions WHERE {GC_DECISIONS_WHERE}"),
            params![older_than_secs],
        )?;
        Ok(n)
    }

    /// Count what [`Store::gc_decisions`] would delete, without deleting —
    /// the dry-run twin (`indexa prune --dry-run`). Shares the WHERE clause so
    /// the two can never drift apart.
    pub fn gc_decisions_count(&self, older_than_secs: i64) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM decisions WHERE {GC_DECISIONS_WHERE}"),
            params![older_than_secs],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

/// Shared GC candidate predicate — see [`Store::gc_decisions`] for the
/// conservative rationale. `?1` = horizon in seconds.
const GC_DECISIONS_WHERE: &str = "status IN ('expired', 'dismissed')
    AND COALESCE(decided_at, created_at) < (unixepoch() - ?1)
    AND id NOT IN (SELECT parent_id FROM decisions WHERE parent_id IS NOT NULL)
    AND id NOT IN (SELECT superseded_by FROM decisions
                    WHERE superseded_by IS NOT NULL)";

fn decision_status(tx: &Transaction, id: i64) -> Result<Option<String>> {
    tx.query_row(
        "SELECT status FROM decisions WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}
