//! Idempotent projections: apply a decided ledger row onto the domain tables.
//!
//! The domain tables (classifications, importance_weights) stay authoritative
//! for runtime; a projection only ever expresses the LATEST decided revision,
//! so re-running one (crash repair, revert) converges to the same downstream
//! state. Ledger-owned weight rows carry a `decision:` reason prefix so a
//! revert can release them without touching weights the user set by hand.

use crate::store::{DecisionRecord, Store};
use anyhow::{anyhow, bail, Result};
use serde_json::json;

use super::DecisionType;

/// Reason prefix marking an importance-weight row as ledger-owned.
const DECISION_REASON_PREFIX: &str = "decision:";

/// Apply the projection for one decided row; returns the effects JSON to stamp
/// via `Store::mark_effects_applied`. Must stay idempotent — both the crash
/// repair sweep and `revert` re-run it.
pub fn apply_decision_effects(store: &mut Store, d: &DecisionRecord) -> Result<serde_json::Value> {
    let chosen = d
        .chosen
        .as_deref()
        .ok_or_else(|| anyhow!("decision {} has no answer to project", d.id))?;
    let Some(ty) = DecisionType::parse(&d.decision_type) else {
        bail!(
            "decision {} has unknown type '{}' — written by a newer indexa?",
            d.id,
            d.decision_type
        );
    };
    match ty {
        DecisionType::Classification => {
            if chosen == "ignore" {
                store.ignore_classification(&d.subject)?;
            } else {
                store.confirm_classification(&d.subject, chosen)?;
            }
            Ok(json!({ "classification": chosen }))
        }
        DecisionType::Duplicate => apply_duplicate(store, d, chosen),
    }
}

fn apply_duplicate(
    store: &mut Store,
    d: &DecisionRecord,
    chosen: &str,
) -> Result<serde_json::Value> {
    let params: serde_json::Value = serde_json::from_str(&d.params)
        .map_err(|e| anyhow!("decision {} has malformed params: {e}", d.id))?;
    let paths: Vec<String> = params
        .get("paths")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or_else(|| anyhow!("decision {} params carry no paths", d.id))?;

    if chosen == "keep_all" {
        // Release only ledger-owned silences on the members; weights the user
        // set by hand (no `decision:` reason) survive.
        for member in &paths {
            release_decision_weight(store, member)?;
        }
        return Ok(json!({ "canonical": null, "silenced": [] }));
    }

    let mut silenced: Vec<&String> = Vec::new();
    for member in &paths {
        if member == chosen {
            continue;
        }
        store.set_weight(
            "file",
            member,
            0.0,
            "user",
            Some(&format!(
                "{DECISION_REASON_PREFIX}{} duplicate of {chosen}",
                d.id
            )),
        )?;
        silenced.push(member);
    }
    // A prior revision may have silenced the path that is canonical now.
    release_decision_weight(store, chosen)?;
    Ok(json!({ "canonical": chosen, "silenced": silenced }))
}

/// Delete the file-weight row on `path` iff the ledger owns it.
fn release_decision_weight(store: &mut Store, path: &str) -> Result<()> {
    let owned = store.list_weights(Some("file"))?.into_iter().any(|w| {
        w.target == path
            && w.reason
                .as_deref()
                .is_some_and(|r| r.starts_with(DECISION_REASON_PREFIX))
    });
    if owned {
        store.delete_weight("file", path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A decided record as `decide_and_apply` would hand to the projection.
    fn decided(
        id: i64,
        decision_type: &str,
        subject: &str,
        params: serde_json::Value,
        chosen: &str,
    ) -> DecisionRecord {
        DecisionRecord {
            id,
            decision_type: decision_type.to_owned(),
            subject: subject.to_owned(),
            params: params.to_string(),
            options: "[]".to_owned(),
            auto_value: None,
            chosen: Some(chosen.to_owned()),
            source: Some("user".to_owned()),
            confidence: None,
            evidence_hash: String::new(),
            priority: 50,
            status: "decided".to_owned(),
            parent_id: None,
            superseded_by: None,
            effects: None,
            effects_applied_at: None,
            created_at: 1,
            decided_at: Some(2),
        }
    }

    #[test]
    fn classification_projection_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let d = decided(1, "classification", "/r/proj", json!({}), "work");
        let first = apply_decision_effects(&mut store, &d).unwrap();
        let second = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(first, second);
        let c = store.classification_for("/r/proj").unwrap().unwrap();
        assert_eq!((c.category.as_str(), c.source.as_str()), ("work", "user"));

        let ignore = decided(2, "classification", "/r/junk", json!({}), "ignore");
        apply_decision_effects(&mut store, &ignore).unwrap();
        apply_decision_effects(&mut store, &ignore).unwrap();
        let c = store.classification_for("/r/junk").unwrap().unwrap();
        assert_eq!(c.source, "ignored");
    }

    #[test]
    fn duplicate_projection_is_idempotent_and_owns_its_weights() {
        let mut store = Store::open_in_memory().unwrap();
        let params = json!({"paths": ["/r/a.txt", "/r/b.txt", "/r/c.txt"], "exact": true, "similarity": 1.0});
        let d = decided(7, "duplicate", "/r/a.txt", params.clone(), "/r/b.txt");

        let first = apply_decision_effects(&mut store, &d).unwrap();
        let second = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first,
            json!({"canonical": "/r/b.txt", "silenced": ["/r/a.txt", "/r/c.txt"]})
        );
        let weights = store.list_weights(Some("file")).unwrap();
        assert_eq!(weights.len(), 2);
        for w in &weights {
            assert_eq!(w.weight, 0.0);
            assert_eq!(
                w.reason.as_deref(),
                Some("decision:7 duplicate of /r/b.txt")
            );
        }

        // Re-deciding with a different canonical converges: the new canonical's
        // ledger-owned silence is released, the others re-stamped.
        let d2 = decided(8, "duplicate", "/r/a.txt", params.clone(), "/r/a.txt");
        apply_decision_effects(&mut store, &d2).unwrap();
        apply_decision_effects(&mut store, &d2).unwrap();
        let weights = store.list_weights(Some("file")).unwrap();
        let targets: Vec<&str> = weights.iter().map(|w| w.target.as_str()).collect();
        assert_eq!(targets, vec!["/r/b.txt", "/r/c.txt"]);

        // keep_all releases ledger-owned silences but spares manual weights.
        store
            .set_weight("file", "/r/c.txt", 2.0, "user", None)
            .unwrap();
        let d3 = decided(9, "duplicate", "/r/a.txt", params, "keep_all");
        apply_decision_effects(&mut store, &d3).unwrap();
        apply_decision_effects(&mut store, &d3).unwrap();
        let weights = store.list_weights(Some("file")).unwrap();
        assert_eq!(weights.len(), 1);
        assert_eq!(weights[0].target, "/r/c.txt");
        assert_eq!(weights[0].weight, 2.0);
    }
}
