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

/// Down-weight an archived dir gets in search. Down-weighted, NOT silenced —
/// "archive" means "kept indexed, deprioritized", never "hidden".
const ARCHIVE_WEIGHT: f32 = 0.5;

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
        DecisionType::Archive => apply_archive(store, d, chosen),
        DecisionType::SummaryDrift => apply_summary_drift(store, d, chosen),
        DecisionType::Language => apply_language(store, d, chosen),
        DecisionType::SymbolAmbiguity => apply_symbol_ambiguity(d, chosen),
    }
}

/// Summary-drift projection. `keep_new` = nothing to do (the regenerated
/// summary already landed when the question was raised); `restore_old` =
/// re-write the summary text from the params stash and NULL the row's
/// embedding — see `Store::restore_summary_text` for why the embedding is
/// cleared rather than kept (it embeds the rejected wording) or regenerated
/// (the projection has no embedder, and a re-summarize would just reproduce
/// the drift). Idempotent: re-writing the same text and re-NULLing converge.
fn apply_summary_drift(
    store: &mut Store,
    d: &DecisionRecord,
    chosen: &str,
) -> Result<serde_json::Value> {
    match chosen {
        "keep_new" => Ok(json!({ "summary": "kept_new" })),
        "restore_old" => {
            let params: serde_json::Value = serde_json::from_str(&d.params)
                .map_err(|e| anyhow!("decision {} has malformed params: {e}", d.id))?;
            let old_summary = params
                .get("old_summary")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("decision {} params carry no old_summary", d.id))?;
            let restored = store.restore_summary_text(
                &d.subject,
                old_summary,
                params.get("old_l0").and_then(|v| v.as_str()),
                params.get("old_model").and_then(|v| v.as_str()),
            )?;
            if restored {
                Ok(json!({ "summary": "restored", "embedding": null }))
            } else {
                // The summary row vanished since the question was asked (rm /
                // prune). Nothing to express — a no-op receipt, not an error,
                // so the repair sweep doesn't retry a restore that can never
                // succeed.
                Ok(json!({ "summary": null }))
            }
        }
        other => bail!(
            "decision {} has unknown summary_drift answer '{other}'",
            d.id
        ),
    }
}

/// Language projection: tag the file's chunks with the chosen language;
/// `ignore` projects nothing (the standing "ignore" answer also suppresses the
/// detector's silent re-apply). Idempotent — the UPDATE converges.
fn apply_language(
    store: &mut Store,
    d: &DecisionRecord,
    chosen: &str,
) -> Result<serde_json::Value> {
    if chosen == "ignore" {
        return Ok(json!({ "language": null }));
    }
    let n = store.set_chunks_language(&d.subject, chosen)?;
    Ok(json!({ "language": chosen, "chunks": n }))
}

/// Symbol-ambiguity projection: the answer IS the artifact — no domain table
/// changes. The effects JSON records the authoritative definer (`null` for
/// "all"); graph surfaces (who_calls / blast_radius) consult the ledger's
/// latest decided row separately (wired in a follow-up).
fn apply_symbol_ambiguity(_d: &DecisionRecord, chosen: &str) -> Result<serde_json::Value> {
    if chosen == "all" {
        Ok(json!({ "authoritative": null }))
    } else {
        Ok(json!({ "authoritative": chosen }))
    }
}

/// Archive projection. `archive` = confirm the `archive` category + a
/// ledger-owned 0.5 dir weight; `keep_active` = release any ledger-owned dir
/// weight (a prior revision may have archived) and deliberately nothing else —
/// no classification row, because "keep active" is an absence of action, and
/// the bucketed evidence hash re-asks naturally as the dir keeps aging.
fn apply_archive(store: &mut Store, d: &DecisionRecord, chosen: &str) -> Result<serde_json::Value> {
    match chosen {
        "archive" => {
            store.confirm_classification(&d.subject, "archive")?;
            // Same ownership rule as duplicate silencing: a dir weight WITHOUT
            // the `decision:` reason is the user's own intent — never seize it
            // (set_weight upserts unconditionally, which would rebrand the row
            // ledger-owned; a later keep_active would then DELETE it instead
            // of leaving it alone).
            let manual = store.list_weights(Some("dir"))?.into_iter().any(|w| {
                w.target == d.subject
                    && !w
                        .reason
                        .as_deref()
                        .is_some_and(|r| r.starts_with(DECISION_REASON_PREFIX))
            });
            if manual {
                return Ok(json!({
                    "classification": "archive",
                    "weight": null,
                    "kept_manual_weight": [d.subject],
                }));
            }
            store.set_weight(
                "dir",
                &d.subject,
                ARCHIVE_WEIGHT,
                "user",
                Some(&format!("{DECISION_REASON_PREFIX}{} archived", d.id)),
            )?;
            Ok(json!({ "classification": "archive", "weight": ARCHIVE_WEIGHT }))
        }
        "keep_active" => {
            release_decision_weight(store, "dir", &d.subject)?;
            Ok(json!({ "classification": null, "weight": null }))
        }
        // decide_and_apply validates answers against the recorded menu, but the
        // repair sweep replays whatever the DB holds — fail loudly, not loosely.
        other => bail!("decision {} has unknown archive answer '{other}'", d.id),
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
            release_decision_weight(store, "file", member)?;
        }
        return Ok(json!({ "canonical": null, "silenced": [] }));
    }

    // One fetch, per-member lookups: a weight row WITHOUT the `decision:` reason
    // is the user's explicit intent — silencing must not seize or overwrite it
    // (set_weight upserts unconditionally, which would rebrand the row as
    // ledger-owned and a later release would DELETE it instead of restoring it).
    let ledger_owned: std::collections::HashMap<String, bool> = store
        .list_weights(Some("file"))?
        .into_iter()
        .map(|w| {
            let owned = w
                .reason
                .as_deref()
                .is_some_and(|r| r.starts_with(DECISION_REASON_PREFIX));
            (w.target, owned)
        })
        .collect();

    let mut silenced: Vec<&String> = Vec::new();
    let mut kept_manual: Vec<&String> = Vec::new();
    for member in &paths {
        if member == chosen {
            continue;
        }
        if ledger_owned.get(member) == Some(&false) {
            kept_manual.push(member);
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
    release_decision_weight(store, "file", chosen)?;
    let mut effects = json!({ "canonical": chosen, "silenced": silenced });
    if !kept_manual.is_empty() {
        effects["kept_manual_weight"] = json!(kept_manual);
    }
    Ok(effects)
}

/// Delete the `kind`-weight row on `path` iff the ledger owns it.
fn release_decision_weight(store: &mut Store, kind: &str, path: &str) -> Result<()> {
    let owned = store.list_weights(Some(kind))?.into_iter().any(|w| {
        w.target == path
            && w.reason
                .as_deref()
                .is_some_and(|r| r.starts_with(DECISION_REASON_PREFIX))
    });
    if owned {
        store.delete_weight(kind, path)?;
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

    #[test]
    fn archive_projection_is_idempotent_and_owns_its_weight() {
        let mut store = Store::open_in_memory().unwrap();
        let params = json!({"days": 400, "files": 3});
        let d = decided(21, "archive", "/old", params.clone(), "archive");

        let first = apply_decision_effects(&mut store, &d).unwrap();
        let second = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, json!({"classification": "archive", "weight": 0.5}));
        let c = store.classification_for("/old").unwrap().unwrap();
        assert_eq!(
            (c.category.as_str(), c.source.as_str()),
            ("archive", "user")
        );
        let w = store.list_weights(Some("dir")).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].target.as_str(), w[0].weight), ("/old", 0.5));
        assert_eq!(w[0].reason.as_deref(), Some("decision:21 archived"));

        // keep_active (a later revision) releases the ledger-owned weight and
        // deliberately leaves the classification alone — "keep active" is an
        // absence of action, not a category.
        let k = decided(22, "archive", "/old", params, "keep_active");
        let first = apply_decision_effects(&mut store, &k).unwrap();
        let second = apply_decision_effects(&mut store, &k).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, json!({"classification": null, "weight": null}));
        assert!(store.list_weights(Some("dir")).unwrap().is_empty());
        assert_eq!(
            store.classification_for("/old").unwrap().unwrap().category,
            "archive",
            "keep_active must not rewrite or remove the classification"
        );
    }

    #[test]
    fn archiving_never_seizes_a_manual_dir_weight() {
        let mut store = Store::open_in_memory().unwrap();
        // The user boosted the dir by hand BEFORE any archive decision existed.
        store
            .set_weight("dir", "/old", 2.0, "user", Some("my hot project"))
            .unwrap();

        let params = json!({"days": 400, "files": 3});
        let d = decided(31, "archive", "/old", params.clone(), "archive");
        let fx = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(fx["kept_manual_weight"], json!(["/old"]));
        assert_eq!(fx["weight"], json!(null));

        // Untouched — value AND reason; keep_active later must spare it too.
        let k = decided(32, "archive", "/old", params, "keep_active");
        apply_decision_effects(&mut store, &k).unwrap();
        let w = store.list_weights(Some("dir")).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].target.as_str(), w[0].weight), ("/old", 2.0));
        assert_eq!(w[0].reason.as_deref(), Some("my hot project"));
    }

    #[test]
    fn summary_drift_restore_rewrites_text_and_clears_the_embedding() {
        let mut store = Store::open_in_memory().unwrap();
        // The row currently holds the NEW (drifted) summary + its embedding.
        store
            .upsert_summary(&crate::store::SummaryRecord {
                path: "/r/f.txt".into(),
                kind: "file".into(),
                parent_path: Some("/r".into()),
                depth: 1,
                summary: "New drifted summary.".into(),
                summary_l0: None,
                embedding: Some(vec![0.0, 1.0]),
                child_count: 0,
                byte_size: 10,
                model: "m2".into(),
                source_hash: "H".into(),
                generated_at: 5,
            })
            .unwrap();
        let params = json!({
            "old_summary": "Old summary. More detail.",
            "old_l0": "Old summary.",
            "new_l0": "New drifted summary.",
            "cosine": 0.1, "old_model": "m1", "new_model": "m2",
        });
        let d = decided(
            41,
            "summary_drift",
            "/r/f.txt",
            params.clone(),
            "restore_old",
        );

        let first = apply_decision_effects(&mut store, &d).unwrap();
        let second = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(first, second, "projection must be idempotent");
        assert_eq!(first, json!({"summary": "restored", "embedding": null}));
        let row = store.summary_by_path("/r/f.txt").unwrap().unwrap();
        assert_eq!(row.summary, "Old summary. More detail.");
        assert_eq!(row.summary_l0.as_deref(), Some("Old summary."));
        assert!(
            row.embedding.is_none(),
            "the rejected wording's embedding must not rank the restored text"
        );
        assert_eq!(row.model, "m1", "provenance follows the restored text");
        assert_eq!(
            row.source_hash, "H",
            "hash stays — the restored text describes the same bytes, and \
             clearing it would re-run the very regeneration the user rejected"
        );

        // keep_new is a recorded no-op.
        let k = decided(42, "summary_drift", "/r/f.txt", params.clone(), "keep_new");
        assert_eq!(
            apply_decision_effects(&mut store, &k).unwrap(),
            json!({"summary": "kept_new"})
        );

        // A vanished row (rm/prune) is a no-op receipt, never a repair loop.
        let gone = decided(43, "summary_drift", "/r/gone.txt", params, "restore_old");
        assert_eq!(
            apply_decision_effects(&mut store, &gone).unwrap(),
            json!({"summary": null})
        );
    }

    #[test]
    fn language_projection_tags_chunks_and_ignore_is_a_noop() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_chunks(&[crate::store::ChunkRecord {
                entry_path: "/r/x.rb".into(),
                seq: 0,
                heading: String::new(),
                text: "puts 1".into(),
                language: None,
                embedding: None,
                embed_model: None,
                content_hash: None,
            }])
            .unwrap();
        let d = decided(51, "language", "/r/x.rb", json!({}), "ruby");
        let first = apply_decision_effects(&mut store, &d).unwrap();
        let second = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, json!({"language": "ruby", "chunks": 1}));
        assert!(store.unlabeled_chunk_files(1, 10).unwrap().is_empty());

        let ig = decided(52, "language", "/r/x.rb", json!({}), "ignore");
        assert_eq!(
            apply_decision_effects(&mut store, &ig).unwrap(),
            json!({"language": null})
        );
    }

    #[test]
    fn symbol_projection_records_the_choice_only() {
        let mut store = Store::open_in_memory().unwrap();
        let params = json!({"definers": ["/a.rs", "/b.rs"], "callers": 3});
        let d = decided(61, "symbol_ambiguity", "foo", params.clone(), "/a.rs");
        assert_eq!(
            apply_decision_effects(&mut store, &d).unwrap(),
            json!({"authoritative": "/a.rs"})
        );
        let all = decided(62, "symbol_ambiguity", "foo", params, "all");
        assert_eq!(
            apply_decision_effects(&mut store, &all).unwrap(),
            json!({"authoritative": null})
        );
    }

    #[test]
    fn silencing_never_seizes_a_pre_existing_manual_weight() {
        let mut store = Store::open_in_memory().unwrap();
        // The user boosted b.txt by hand BEFORE any duplicate decision existed.
        store
            .set_weight("file", "/r/b.txt", 3.0, "user", Some("my important copy"))
            .unwrap();

        let params = json!({"paths": ["/r/a.txt", "/r/b.txt"], "exact": true, "similarity": 1.0});
        let d = decided(11, "duplicate", "/r/a.txt", params.clone(), "/r/a.txt");
        let fx = apply_decision_effects(&mut store, &d).unwrap();
        assert_eq!(fx["kept_manual_weight"], json!(["/r/b.txt"]));
        assert_eq!(fx["silenced"], json!([]));

        // The manual weight is untouched — value AND reason.
        let w = store.list_weights(Some("file")).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].target.as_str(), w[0].weight), ("/r/b.txt", 3.0));
        assert_eq!(w[0].reason.as_deref(), Some("my important copy"));

        // keep_all later must also leave it alone (not delete it as ledger-owned).
        let d2 = decided(12, "duplicate", "/r/a.txt", params, "keep_all");
        apply_decision_effects(&mut store, &d2).unwrap();
        let w = store.list_weights(Some("file")).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].weight, 3.0);
    }
}
