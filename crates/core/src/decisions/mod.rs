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
    if d.status != "open" {
        bail!("decision {id} is '{}', not open", d.status);
    }
    // Validate BEFORE the answer commits: an unknown type or off-menu answer
    // would otherwise leave a decided row whose projection can never succeed.
    if DecisionType::parse(&d.decision_type).is_none() {
        bail!(
            "decision {id} has unknown type '{}' — answer it with a newer indexa",
            d.decision_type
        );
    }
    // Fail CLOSED on missing/unreadable options: every open question is created
    // with a populated menu, so an empty or corrupt one means the row can't be
    // safely interpreted — accepting a free-form `chosen` here would project an
    // arbitrary string into the domain tables.
    let options: Vec<String> = serde_json::from_str(&d.options)
        .map_err(|e| anyhow!("decision {id} has unreadable options ({e}) — refusing to answer"))?;
    if options.is_empty() {
        bail!("decision {id} has no recorded options — refusing a free-form answer");
    }
    if !options.iter().any(|o| o == chosen) {
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

/// How a fresh Tier-0 suggestion was routed through the ledger's
/// revision-chain rules — see [`route_uncertain_classification`].
#[derive(Debug, PartialEq, Eq)]
pub enum SuggestionOutcome {
    /// A standing answer for the subject still applies; its projection was
    /// restored without asking. Carries the restored answer value.
    Restored(String),
    /// A question was opened — parentless, or chained to the prior revision
    /// when one exists. Carries the new row id.
    Opened(i64),
    /// An open or sticky-dismissed row already covers this — nothing inserted.
    Deduped,
    /// Confident suggestion outside the question band — the ledger stays out of it.
    Skipped,
}

/// Route a fresh Tier-0 classification suggestion through the ledger when the
/// classifications table has NO live user row for the subject.
///
/// The ledger — not the classifications table — holds the user's standing
/// answer: a `classifications` row vanishes with its entry (`indexa rm`), but
/// decided revisions persist by design. Three cases:
/// - prior decided revision AGREES (or said "ignore") → silently re-project it
///   (the user's answer is restored, no question);
/// - prior CONTRADICTS the fresh suggestion → keep the prior projected (it
///   stays authoritative) and open a re-ask CHAINED to it (priority 100) —
///   never a parentless second head for the key;
/// - no prior → open an uncertainty question only inside the
///   [`detectors::UNCERTAINTY_FLOOR`, `below`) confidence band.
pub fn route_uncertain_classification(
    store: &mut Store,
    subject: &str,
    category: &str,
    confidence: f32,
    evidence_hash: String,
    options: serde_json::Value,
    below: f32,
) -> Result<SuggestionOutcome> {
    use crate::store::NewDecision;
    if let Some(prior) = store.latest_decided(DecisionType::Classification.as_str(), subject)? {
        let standing = prior.chosen.clone().unwrap_or_default();
        if standing == "ignore" {
            store.ignore_classification(subject)?;
            return Ok(SuggestionOutcome::Restored(standing));
        }
        if !standing.is_empty() && standing == category {
            store.confirm_classification(subject, &standing)?;
            return Ok(SuggestionOutcome::Restored(standing));
        }
        if !standing.is_empty() {
            // Contradiction. The standing answer remains authoritative until the
            // re-ask is resolved, so restore its projection alongside the question.
            store.confirm_classification(subject, &standing)?;
            let opened = store.supersede_with(
                prior.id,
                NewDecision {
                    decision_type: DecisionType::Classification.as_str().to_owned(),
                    subject: subject.to_owned(),
                    params: serde_json::json!({
                        "category": category,
                        "confidence": confidence,
                        "prior": {"chosen": standing, "decided_at": prior.decided_at},
                    }),
                    options,
                    auto_value: Some(category.to_owned()),
                    confidence: Some(confidence),
                    evidence_hash,
                    priority: 100,
                    paths: vec![subject.to_owned()],
                },
            )?;
            return Ok(match opened {
                Some(id) => SuggestionOutcome::Opened(id),
                None => SuggestionOutcome::Deduped,
            });
        }
    }
    if confidence < detectors::UNCERTAINTY_FLOOR || confidence >= below {
        return Ok(SuggestionOutcome::Skipped);
    }
    let opened = store.record_decision(crate::store::NewDecision {
        decision_type: DecisionType::Classification.as_str().to_owned(),
        subject: subject.to_owned(),
        params: serde_json::json!({"category": category, "confidence": confidence}),
        options,
        auto_value: Some(category.to_owned()),
        confidence: Some(confidence),
        evidence_hash,
        priority: 50,
        paths: vec![subject.to_owned()],
    })?;
    Ok(match opened {
        Some(id) => SuggestionOutcome::Opened(id),
        None => SuggestionOutcome::Deduped,
    })
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
    fn repair_never_resurrects_a_superseded_answer() {
        // Crash before P's projection → re-ask C answered → repair must NOT
        // re-apply P's stale answer over C's (the superseded row is excluded
        // from the sweep even though its receipt is missing).
        let mut store = Store::open_in_memory().unwrap();
        let p = classification_question(&mut store, "/r/proj");
        store.answer_decision(p, "work", "user").unwrap(); // crash: no receipt
        let c = store
            .supersede_with(
                p,
                NewDecision {
                    decision_type: "classification".into(),
                    subject: "/r/proj".into(),
                    params: json!({"category": "code", "confidence": 0.7,
                                   "prior": {"chosen": "work", "decided_at": 1}}),
                    options: json!(["work", "code", "ignore"]),
                    auto_value: Some("code".into()),
                    confidence: Some(0.7),
                    evidence_hash: "fp2".into(),
                    priority: 100,
                    paths: vec!["/r/proj".into()],
                },
            )
            .unwrap()
            .unwrap();
        decide_and_apply(&mut store, c, "code", "user").unwrap();
        assert_eq!(
            store
                .classification_for("/r/proj")
                .unwrap()
                .unwrap()
                .category,
            "code"
        );

        // The sweep sees no repairable rows (P is superseded) and the
        // projection still expresses C's answer afterwards.
        assert_eq!(repair_unapplied(&mut store).unwrap(), 0);
        assert_eq!(
            store
                .classification_for("/r/proj")
                .unwrap()
                .unwrap()
                .category,
            "code",
            "repair must not resurrect the superseded 'work' answer"
        );
    }

    #[test]
    fn route_restores_standing_answer_instead_of_asking_again() {
        let mut store = Store::open_in_memory().unwrap();
        let id = classification_question(&mut store, "/r/proj");
        decide_and_apply(&mut store, id, "work", "user").unwrap();
        // Simulate the classifications row vanishing with its entry (indexa rm).
        store.delete_classification("/r/proj").unwrap();

        let out = route_uncertain_classification(
            &mut store,
            "/r/proj",
            "work",
            0.7,
            "fp2".into(),
            json!(["work", "code", "ignore"]),
            0.8,
        )
        .unwrap();
        assert_eq!(out, SuggestionOutcome::Restored("work".into()));
        let c = store.classification_for("/r/proj").unwrap().unwrap();
        assert_eq!((c.category.as_str(), c.source.as_str()), ("work", "user"));
        assert_eq!(store.open_decision_count().unwrap(), 0, "no question asked");
    }

    #[test]
    fn route_chains_contradiction_to_prior_never_a_second_head() {
        let mut store = Store::open_in_memory().unwrap();
        let p = classification_question(&mut store, "/r/proj");
        decide_and_apply(&mut store, p, "work", "user").unwrap();
        store.delete_classification("/r/proj").unwrap();

        // Fresh evidence contradicts the standing answer → chained re-ask.
        let out = route_uncertain_classification(
            &mut store,
            "/r/proj",
            "code",
            0.7,
            "fp2".into(),
            json!(["work", "code", "ignore"]),
            0.8,
        )
        .unwrap();
        let SuggestionOutcome::Opened(c) = out else {
            panic!("expected a chained re-ask, got {out:?}");
        };
        let child = store.decision_by_id(c).unwrap().unwrap();
        assert_eq!(child.parent_id, Some(p), "must chain to the prior revision");
        assert_eq!(child.priority, 100);
        // Standing answer stays projected while the re-ask is pending.
        let cls = store.classification_for("/r/proj").unwrap().unwrap();
        assert_eq!(
            (cls.category.as_str(), cls.source.as_str()),
            ("work", "user")
        );

        // Resolving the re-ask leaves exactly ONE live head for the key.
        decide_and_apply(&mut store, c, "code", "user").unwrap();
        let prior = store.decision_by_id(p).unwrap().unwrap();
        assert_eq!(prior.superseded_by, Some(c));
        let head = store
            .latest_decided("classification", "/r/proj")
            .unwrap()
            .unwrap();
        assert_eq!(head.id, c);
        assert_eq!(
            store
                .decision_history("classification", "/r/proj")
                .unwrap()
                .iter()
                .filter(|d| d.status == "decided" && d.superseded_by.is_none())
                .count(),
            1,
            "exactly one un-superseded decided revision"
        );
    }

    #[test]
    fn route_without_prior_respects_the_confidence_band() {
        let mut store = Store::open_in_memory().unwrap();
        let confident = route_uncertain_classification(
            &mut store,
            "/r/sure",
            "code",
            0.9,
            "fp".into(),
            json!(["code", "ignore"]),
            0.8,
        )
        .unwrap();
        assert_eq!(confident, SuggestionOutcome::Skipped);
        let uncertain = route_uncertain_classification(
            &mut store,
            "/r/hmm",
            "code",
            0.65,
            "fp".into(),
            json!(["code", "ignore"]),
            0.8,
        )
        .unwrap();
        assert!(matches!(uncertain, SuggestionOutcome::Opened(_)));
    }

    #[test]
    fn decide_and_apply_fails_closed_on_empty_or_corrupt_options() {
        let mut store = Store::open_in_memory().unwrap();
        let empty = store
            .record_decision(NewDecision {
                decision_type: "classification".into(),
                subject: "/r/empty".into(),
                params: json!({}),
                options: json!([]),
                auto_value: None,
                confidence: None,
                evidence_hash: "fp".into(),
                priority: 50,
                paths: vec![],
            })
            .unwrap()
            .unwrap();
        assert!(
            decide_and_apply(&mut store, empty, "anything", "user").is_err(),
            "empty options must refuse free-form answers"
        );
        assert_eq!(
            store.decision_by_id(empty).unwrap().unwrap().status,
            "open",
            "nothing committed on refusal"
        );
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
