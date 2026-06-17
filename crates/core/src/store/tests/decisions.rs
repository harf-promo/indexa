use super::*;

// ── Decision Ledger (v0.22) ───────────────────────────────────────────────────

#[test]
fn decision_lifecycle_and_inbox_order() {
    let mut store = Store::open_in_memory().unwrap();
    let id_low = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap()
        .unwrap();
    let mut high = new_decision("duplicate", "dup:abc");
    high.priority = 100;
    let id_high = store.record_decision(high).unwrap().unwrap();

    // Inbox order is priority DESC (the re-ask=100 > duplicate=60 > classification=50 ladder).
    let open = store.open_decisions(None, 10).unwrap();
    assert_eq!(
        open.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![id_high, id_low]
    );
    assert_eq!(store.open_decision_count().unwrap(), 2);
    let only_dup = store.open_decisions(Some("duplicate"), 10).unwrap();
    assert_eq!(only_dup.len(), 1);
    assert_eq!(only_dup[0].id, id_high);

    store.answer_decision(id_low, "docs", "user").unwrap();
    let d = store.decision_by_id(id_low).unwrap().unwrap();
    assert_eq!(d.status, "decided");
    assert_eq!(d.chosen.as_deref(), Some("docs"));
    assert_eq!(d.source.as_deref(), Some("user"));
    assert!(d.decided_at.is_some(), "answer must stamp decided_at");
    assert_eq!(store.open_decision_count().unwrap(), 1);

    // The fill-in-place transition happens exactly once: re-answering errors.
    assert!(store.answer_decision(id_low, "code", "user").is_err());
    assert!(store.answer_decision(9999, "code", "user").is_err());
}

#[test]
fn decision_open_dedup_via_partial_unique() {
    let mut store = Store::open_in_memory().unwrap();
    let first = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap();
    assert!(first.is_some());
    // Same key while a question is open → swallowed (racing detectors are safe).
    let dup = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap();
    assert!(
        dup.is_none(),
        "second open question for same key must dedup"
    );
    assert_eq!(store.open_decision_count().unwrap(), 1);
    // A different subject (or type) is a different question.
    assert!(store
        .record_decision(new_decision("classification", "/proj/b"))
        .unwrap()
        .is_some());
    assert!(store
        .record_decision(new_decision("duplicate", "/proj/a"))
        .unwrap()
        .is_some());
}

#[test]
fn decision_dismissal_is_sticky_until_evidence_changes() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap()
        .unwrap();
    store.dismiss_decision(id).unwrap();
    let d = store.decision_by_id(id).unwrap().unwrap();
    assert_eq!(
        (d.status.as_str(), d.source.as_deref()),
        ("dismissed", Some("system"))
    );
    assert!(d.decided_at.is_some());

    // Same evidence → the question stays dismissed.
    assert!(store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap()
        .is_none());
    // Changed evidence → the question legitimately returns.
    let mut changed = new_decision("classification", "/proj/a");
    changed.evidence_hash = "h2".to_owned();
    assert!(store.record_decision(changed).unwrap().is_some());

    // Dismissing a non-open row errors.
    assert!(store.dismiss_decision(id).is_err());
}

#[test]
fn decision_revision_chain_supersede_and_latest() {
    let mut store = Store::open_in_memory().unwrap();
    let id1 = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap()
        .unwrap();
    store.answer_decision(id1, "docs", "user").unwrap();

    // Re-ask on new evidence: the prior answer stays authoritative until the
    // new revision is answered.
    let mut reask = new_decision("classification", "/proj/a");
    reask.evidence_hash = "h2".to_owned();
    let id2 = store.supersede_with(id1, reask).unwrap().unwrap();
    assert_eq!(
        store
            .latest_decided("classification", "/proj/a")
            .unwrap()
            .unwrap()
            .id,
        id1
    );

    store.answer_decision(id2, "code", "user").unwrap();
    let prior = store.decision_by_id(id1).unwrap().unwrap();
    assert_eq!(
        prior.superseded_by,
        Some(id2),
        "answering the re-ask stamps the prior row"
    );
    let latest = store
        .latest_decided("classification", "/proj/a")
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, id2);
    assert_eq!(latest.chosen.as_deref(), Some("code"));

    let history = store.decision_history("classification", "/proj/a").unwrap();
    assert_eq!(
        history.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![id1, id2]
    );
    assert_eq!(history[1].parent_id, Some(id1));
}

#[test]
fn decisions_survive_subtree_delete() {
    // Documented design (see store::schema + store::decisions): decisions are
    // standing user intent and are NOT cleared with the entry, like weights.
    let mut store = Store::open_in_memory().unwrap();
    seed_full_entry(&mut store, "/proj/sub/a.rs");
    let id = store
        .record_decision(new_decision("classification", "/proj/sub/a.rs"))
        .unwrap()
        .unwrap();

    store.delete_subtree("/proj/sub").unwrap();

    assert_eq!(orphan_rows_for(&store, "/proj/sub/a.rs"), 0);
    assert!(
        store.decision_by_id(id).unwrap().is_some(),
        "decision must survive"
    );
    assert_eq!(
        store.decisions_touching_path("/proj/sub/a.rs").unwrap(),
        vec![id]
    );
}

#[test]
fn answer_decisions_under_answers_only_matching_open_rows() {
    let mut store = Store::open_in_memory().unwrap();
    let in1 = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap()
        .unwrap();
    let in2 = store
        .record_decision(new_decision("classification", "/proj/b"))
        .unwrap()
        .unwrap();
    let other_dir = store
        .record_decision(new_decision("classification", "/other/c"))
        .unwrap()
        .unwrap();
    let other_type = store
        .record_decision(new_decision("duplicate", "/proj/d"))
        .unwrap()
        .unwrap();
    // An already-decided row under the prefix must not be touched.
    let decided = store
        .record_decision(new_decision("classification", "/proj/e"))
        .unwrap()
        .unwrap();
    store.answer_decision(decided, "docs", "user").unwrap();

    let answered = store
        .answer_decisions_under("/proj/", "classification", "archive", "user")
        .unwrap();
    assert_eq!(answered, vec![in1, in2]);
    for id in [in1, in2] {
        let d = store.decision_by_id(id).unwrap().unwrap();
        assert_eq!(
            (d.status.as_str(), d.chosen.as_deref()),
            ("decided", Some("archive"))
        );
    }
    assert_eq!(
        store.decision_by_id(other_dir).unwrap().unwrap().status,
        "open"
    );
    assert_eq!(
        store.decision_by_id(other_type).unwrap().unwrap().status,
        "open"
    );
    assert_eq!(
        store
            .decision_by_id(decided)
            .unwrap()
            .unwrap()
            .chosen
            .as_deref(),
        Some("docs")
    );
}

#[test]
fn unapplied_decided_and_mark_effects_applied_roundtrip() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .record_decision(new_decision("classification", "/proj/a"))
        .unwrap()
        .unwrap();
    // Open rows are not repair targets.
    assert!(store.unapplied_decided(10).unwrap().is_empty());

    store.answer_decision(id, "docs", "user").unwrap();
    let pending = store.unapplied_decided(10).unwrap();
    assert_eq!(
        pending.len(),
        1,
        "decided-but-unprojected row is a repair target"
    );
    assert_eq!(pending[0].id, id);

    let effects = serde_json::json!({"classification": {"path": "/proj/a", "category": "docs"}});
    store.mark_effects_applied(id, &effects).unwrap();
    assert!(store.unapplied_decided(10).unwrap().is_empty());
    let d = store.decision_by_id(id).unwrap().unwrap();
    assert_eq!(d.effects.as_deref(), Some(effects.to_string().as_str()));
    assert!(d.effects_applied_at.is_some());
}

#[test]
fn gc_decisions_never_deletes_open_or_live_chain_rows() {
    let mut store = Store::open_in_memory().unwrap();
    // Old open row — never a GC candidate regardless of age.
    let open_id = store
        .record_decision(new_decision("classification", "/open"))
        .unwrap()
        .unwrap();
    // Old decided (current) row — never a GC candidate.
    let decided_id = store
        .record_decision(new_decision("classification", "/decided"))
        .unwrap()
        .unwrap();
    store.answer_decision(decided_id, "docs", "user").unwrap();
    // Old dismissed + expired rows — GC candidates.
    let dismissed_id = store
        .record_decision(new_decision("classification", "/dismissed"))
        .unwrap()
        .unwrap();
    store.dismiss_decision(dismissed_id).unwrap();
    let expired_id = store
        .record_decision(new_decision("classification", "/expired"))
        .unwrap()
        .unwrap();
    store.expire_decision(expired_id, "path vanished").unwrap();
    let e = store.decision_by_id(expired_id).unwrap().unwrap();
    assert_eq!(
        (e.status.as_str(), e.source.as_deref()),
        ("expired", Some("system"))
    );
    assert!(
        e.params.contains("path vanished"),
        "expiry note recorded in params"
    );
    // Old dismissed row that is the parent of a live re-ask — chain-referenced, kept.
    let chained_id = store
        .record_decision(new_decision("classification", "/chained"))
        .unwrap()
        .unwrap();
    store.dismiss_decision(chained_id).unwrap();
    let mut reask = new_decision("classification", "/chained");
    reask.evidence_hash = "h2".to_owned();
    let reask_id = store.supersede_with(chained_id, reask).unwrap().unwrap();

    // Backdate everything past the horizon; eligibility is decided by age + status + chain refs.
    store
        .db_connection()
        .execute(
            "UPDATE decisions SET created_at = 0, decided_at =
                 CASE WHEN decided_at IS NULL THEN NULL ELSE 0 END",
            [],
        )
        .unwrap();

    let removed = store.gc_decisions(86_400).unwrap();
    assert_eq!(
        removed, 2,
        "only the unreferenced dismissed + expired rows go"
    );
    assert!(store.decision_by_id(dismissed_id).unwrap().is_none());
    assert!(store.decision_by_id(expired_id).unwrap().is_none());
    assert!(
        store.decision_by_id(open_id).unwrap().is_some(),
        "open survives"
    );
    assert!(
        store.decision_by_id(decided_id).unwrap().is_some(),
        "current answer survives"
    );
    assert!(
        store.decision_by_id(chained_id).unwrap().is_some(),
        "chain parent survives"
    );
    assert!(store.decision_by_id(reask_id).unwrap().is_some());

    // A recent dismissed row is inside the horizon — kept.
    let recent_id = store
        .record_decision(new_decision("classification", "/recent"))
        .unwrap()
        .unwrap();
    store.dismiss_decision(recent_id).unwrap();
    assert_eq!(store.gc_decisions(86_400).unwrap(), 0);
    assert!(store.decision_by_id(recent_id).unwrap().is_some());
}

#[test]
fn decision_paths_cascade_on_decision_delete() {
    let mut store = Store::open_in_memory().unwrap();
    let mut d = new_decision("duplicate", "dup:abc");
    d.paths = vec!["/proj/a.rs".to_owned(), "/proj/b.rs".to_owned()];
    let id = store.record_decision(d).unwrap().unwrap();
    assert_eq!(
        store.decisions_touching_path("/proj/a.rs").unwrap(),
        vec![id]
    );
    assert_eq!(
        store.decisions_touching_path("/proj/b.rs").unwrap(),
        vec![id]
    );
    // Exact match only — no prefix semantics on decision_paths.
    assert!(store.decisions_touching_path("/proj").unwrap().is_empty());

    // Dismissed rows no longer "touch" their paths (only open/decided-current do).
    store.dismiss_decision(id).unwrap();
    assert!(store
        .decisions_touching_path("/proj/a.rs")
        .unwrap()
        .is_empty());

    // Deleting the decision (via GC) cascades its decision_paths rows.
    store
        .db_connection()
        .execute("UPDATE decisions SET created_at = 0, decided_at = 0", [])
        .unwrap();
    assert_eq!(store.gc_decisions(86_400).unwrap(), 1);
    let remaining: i64 = store
        .db_connection()
        .query_row(
            "SELECT COUNT(*) FROM decision_paths WHERE decision_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining, 0,
        "decision_paths rows must cascade with the decision"
    );
}

#[test]
fn health_stats_empty_index_is_all_zeros() {
    let store = Store::open_in_memory().unwrap();
    let h = store.health_stats().unwrap();
    assert_eq!(h.files, 0);
    assert_eq!(h.dirs, 0);
    assert_eq!(h.files_with_chunks, 0);
    assert_eq!(h.chunks, 0);
    assert_eq!(h.embedded_chunks, 0);
    assert_eq!(h.files_summarized, 0);
    assert_eq!(h.dirs_summarized, 0);
    assert_eq!(h.stale_summaries, 0);
}

#[test]
fn health_stats_joins_live_entries_and_flags_stale() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/p/a.rs", EntryKind::File, 10),
            dummy_entry("/p/b.rs", EntryKind::File, 10),
            dummy_entry("/p", EntryKind::Dir, 0),
        ])
        .unwrap();
    // One file deep-indexed with a mixed pair: one embedded chunk, one not.
    store
        .upsert_chunks(&[
            dummy_chunk_embedded("/p/a.rs", 0, "fn a() {}"),
            dummy_chunk("/p/a.rs", 1, "fn b() {}"),
        ])
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/p/a.rs", "file", Some("/p"), 1))
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/p", "dir", None, 0))
        .unwrap();
    // Orphan summary (no entries row, as after a root removal): the entries
    // join must exclude it, or coverage could exceed 100%.
    store
        .upsert_summary(&dummy_summary("/gone/x.rs", "file", Some("/gone"), 1))
        .unwrap();
    // Stale = the file changed after its summary was written. Only /p/a.rs
    // qualifies: /p keeps modified_s NULL and must not count.
    store
        .db_connection()
        .execute_batch(
            "UPDATE summaries SET generated_at = 100;
             UPDATE entries SET modified_s = 200 WHERE path = '/p/a.rs';",
        )
        .unwrap();

    let h = store.health_stats().unwrap();
    assert_eq!(h.files, 2);
    assert_eq!(h.dirs, 1);
    assert_eq!(h.files_with_chunks, 1);
    assert_eq!(h.chunks, 2);
    assert_eq!(h.embedded_chunks, 1);
    assert_eq!(h.files_summarized, 1, "orphan summary must be excluded");
    assert_eq!(h.dirs_summarized, 1);
    assert_eq!(h.stale_summaries, 1);
}
