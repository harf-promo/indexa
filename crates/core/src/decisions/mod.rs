//! Decision Ledger domain logic (v0.22), layered over [`crate::store`]'s
//! `decisions`/`decision_paths` tables.
//!
//! - [`detectors`] — passes that turn uncertainty signals into open questions.
//! - [`effects`] — idempotent projections of an answer onto the domain tables
//!   (classifications, importance_weights), which stay authoritative for runtime.
//! - [`templates`] — late rendering of structured params into a displayable
//!   question, shared by CLI/MCP/web.
//!
//! Crash-safety contract (see store::decisions): the ledger row is answered and
//! committed FIRST, then the projection runs and stamps `effects_applied_at`.
//! A decided row without the stamp is repaired by [`repair_unapplied`], which
//! is safe because every projection is idempotent.

pub mod detectors;
pub mod effects;
pub mod templates;

use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use std::fmt;

/// A decision's type — validated in Rust, deliberately not by a DB CHECK
/// (widening the `edges.kind` CHECK once cost a table-recreate migration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionType {
    /// "What semantic category is this folder?" (subject = the dir path).
    Classification,
    /// "These files are copies — which is canonical?" (subject = first member path).
    Duplicate,
}

impl DecisionType {
    /// The stable wire/string form (used in the DB, CLI, and API).
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionType::Classification => "classification",
            DecisionType::Duplicate => "duplicate",
        }
    }

    /// Parse a wire string back into a type (rows written by a newer binary may
    /// carry types this one does not know — callers must tolerate `None`).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "classification" => DecisionType::Classification,
            "duplicate" => DecisionType::Duplicate,
            _ => return None,
        })
    }

    /// Every type, for UI filters and `--type` validation.
    pub const ALL: [DecisionType; 2] = [DecisionType::Classification, DecisionType::Duplicate];
}

impl fmt::Display for DecisionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Answer an open question and project the answer onto the domain tables.
/// The single entry point every surface (CLI/MCP/web) routes answers through.
///
/// Order is the crash-safety contract: the ledger row commits first (the answer
/// is never lost), the idempotent projection runs second, the receipt stamp
/// last — a crash between the steps leaves a repairable row, never a lie.
/// Returns the effects JSON that was applied.
pub fn decide_and_apply(
    store: &mut Store,
    id: i64,
    chosen: &str,
    source: &str,
) -> Result<serde_json::Value> {
    let d = store
        .decision_by_id(id)?
        .ok_or_else(|| anyhow!("no decision with id {id}"))?;
    // Validate BEFORE the answer commits: an unknown type or off-menu answer
    // would otherwise leave a decided row whose projection can never succeed.
    if DecisionType::parse(&d.decision_type).is_none() {
        bail!(
            "decision {id} has unknown type '{}' — answer it with a newer indexa",
            d.decision_type
        );
    }
    let options: Vec<String> = serde_json::from_str(&d.options).unwrap_or_default();
    if !options.is_empty() && !options.iter().any(|o| o == chosen) {
        bail!(
            "'{chosen}' is not an option for decision {id} (options: {})",
            options.join(", ")
        );
    }
    store.answer_decision(id, chosen, source)?;
    let d = store
        .decision_by_id(id)?
        .ok_or_else(|| anyhow!("no decision with id {id}"))?;
    let effects = effects::apply_decision_effects(store, &d)?;
    store.mark_effects_applied(id, &effects)?;
    Ok(effects)
}

/// Crash recovery: re-run the projection for decided rows whose receipt was
/// never stamped (single bounded pass; a poisoned row is logged and left for
/// the next sweep rather than blocking the rest). Returns rows repaired.
pub fn repair_unapplied(store: &mut Store) -> Result<usize> {
    let mut repaired = 0;
    for d in store.unapplied_decided(500)? {
        match effects::apply_decision_effects(store, &d) {
            Ok(effects) => {
                store.mark_effects_applied(d.id, &effects)?;
                repaired += 1;
            }
            Err(e) => tracing::warn!("repairing decision {}: {e:#}", d.id),
        }
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewDecision;
    use serde_json::json;

    fn classification_question(store: &mut Store, subject: &str) -> i64 {
        store
            .record_decision(NewDecision {
                decision_type: DecisionType::Classification.as_str().to_owned(),
                subject: subject.to_owned(),
                params: json!({"category": "code", "confidence": 0.7}),
                options: json!(["work", "code", "ignore"]),
                auto_value: Some("code".to_owned()),
                confidence: Some(0.7),
                evidence_hash: "fp1".to_owned(),
                priority: 50,
                paths: vec![subject.to_owned()],
            })
            .unwrap()
            .unwrap()
    }

    #[test]
    fn wire_roundtrip_is_stable() {
        for t in DecisionType::ALL {
            assert_eq!(DecisionType::parse(t.as_str()), Some(t));
        }
        assert_eq!(DecisionType::parse("nonsense"), None);
    }

    #[test]
    fn decide_and_apply_answers_projects_and_stamps() {
        let mut store = Store::open_in_memory().unwrap();
        let id = classification_question(&mut store, "/r/proj");

        let effects = decide_and_apply(&mut store, id, "work", "user").unwrap();
        assert_eq!(effects, json!({"classification": "work"}));

        let d = store.decision_by_id(id).unwrap().unwrap();
        assert_eq!(d.status, "decided");
        assert_eq!(d.chosen.as_deref(), Some("work"));
        assert!(d.effects_applied_at.is_some());
        let c = store.classification_for("/r/proj").unwrap().unwrap();
        assert_eq!((c.category.as_str(), c.source.as_str()), ("work", "user"));
    }

    #[test]
    fn decide_and_apply_rejects_off_menu_answers_before_committing() {
        let mut store = Store::open_in_memory().unwrap();
        let id = classification_question(&mut store, "/r/proj");
        assert!(decide_and_apply(&mut store, id, "banana", "user").is_err());
        // The row must still be open — nothing committed.
        assert_eq!(store.decision_by_id(id).unwrap().unwrap().status, "open");
    }

    #[test]
    fn repair_unapplied_reruns_missing_projections() {
        let mut store = Store::open_in_memory().unwrap();
        let id = classification_question(&mut store, "/r/proj");
        // Simulate a crash between answer and projection: decided, no receipt.
        store.answer_decision(id, "code", "user").unwrap();
        assert!(store.classification_for("/r/proj").unwrap().is_none());

        assert_eq!(repair_unapplied(&mut store).unwrap(), 1);
        let c = store.classification_for("/r/proj").unwrap().unwrap();
        assert_eq!(c.category, "code");
        assert!(store
            .decision_by_id(id)
            .unwrap()
            .unwrap()
            .effects_applied_at
            .is_some());
        // Second sweep finds nothing.
        assert_eq!(repair_unapplied(&mut store).unwrap(), 0);
    }

    #[test]
    fn backfill_imports_pre_ledger_answers_once() {
        let mut store = Store::open_in_memory().unwrap();
        store.confirm_classification("/r/work", "work").unwrap();
        store.ignore_classification("/r/junk").unwrap();
        // Auto rows are NOT backfilled — only standing user intent is.
        store
            .upsert_auto_classifications(&[(
                "/r/auto".to_owned(),
                "dir".to_owned(),
                "code".to_owned(),
                0.9,
            )])
            .unwrap();

        assert_eq!(store.backfill_classification_decisions().unwrap(), 2);
        // Idempotent: the guard sees existing classification rows and does nothing.
        assert_eq!(store.backfill_classification_decisions().unwrap(), 0);

        let prior = store
            .latest_decided("classification", "/r/work")
            .unwrap()
            .unwrap();
        assert_eq!(prior.chosen.as_deref(), Some("work"));
        assert_eq!(prior.source.as_deref(), Some("user"));
        // '' = re-askable on the first contradiction (no fingerprint was recorded).
        assert_eq!(prior.evidence_hash, "");
        // Receipt stamped: the domain table already reflects the answer, so the
        // repair sweep must not touch backfilled rows.
        assert!(prior.effects_applied_at.is_some());
        assert_eq!(repair_unapplied(&mut store).unwrap(), 0);

        let ignored = store
            .latest_decided("classification", "/r/junk")
            .unwrap()
            .unwrap();
        assert_eq!(ignored.chosen.as_deref(), Some("ignore"));
        assert!(store
            .latest_decided("classification", "/r/auto")
            .unwrap()
            .is_none());
    }
}
