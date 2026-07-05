use super::*;

// ── Incremental re-summarize: source hashes + stale candidates (v0.24) ────────

#[test]
fn file_source_hash_tracks_content_and_degrades_to_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("a.txt");

    std::fs::write(&f, "hello").unwrap();
    let h1 = file_source_hash(&f);
    assert_eq!(h1.len(), 64, "lowercase-hex SHA-256");
    assert_eq!(h1, file_source_hash(&f), "same bytes ⇒ same hash");

    std::fs::write(&f, "hello!").unwrap();
    assert_ne!(h1, file_source_hash(&f), "changed bytes ⇒ changed hash");

    // Unreadable ⇒ "" (freshness unknown — must never enable a skip).
    assert_eq!(file_source_hash(&tmp.path().join("missing.txt")), "");
}

#[test]
fn dir_source_hash_is_order_independent_and_tracks_children() {
    let a = || {
        let mut r = dummy_summary("/d/a", "file", Some("/d"), 2);
        r.source_hash = "h1".into();
        r
    };
    let b = || {
        let mut r = dummy_summary("/d/b", "file", Some("/d"), 2);
        r.source_hash = "h2".into();
        r
    };

    let fwd = dir_source_hash(&[a(), b()]);
    let rev = dir_source_hash(&[b(), a()]);
    assert!(!fwd.is_empty());
    assert_eq!(fwd, rev, "row order must not change the roll-up hash");

    // A child's hash moving moves the dir hash.
    let mut a2 = a();
    a2.source_hash = "h1-changed".into();
    assert_ne!(dir_source_hash(&[a2, b()]), fwd);

    // Membership change moves the dir hash.
    assert_ne!(dir_source_hash(&[a()]), fwd);

    // Any unhashed child (legacy/unreadable) ⇒ "" — the dir must never skip on
    // a hash it can't trust. Same for no children at all.
    let mut blank = b();
    blank.source_hash = String::new();
    assert_eq!(dir_source_hash(&[a(), blank]), "");
    assert_eq!(dir_source_hash(&[]), "");
}

#[test]
fn stale_summary_candidates_flags_only_mtime_newer_than_summary() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            entry_with_mtime("/r", EntryKind::Dir, 100),
            entry_with_mtime("/r/stale.txt", EntryKind::File, 100),
            entry_with_mtime("/r/fresh.txt", EntryKind::File, 100),
            entry_with_mtime("/r/skipped.txt", EntryKind::File, 100),
            // No summary row at all → not a *stale-summary* candidate (it's a new
            // path, handled by the INSERT OR IGNORE half of enqueue).
            entry_with_mtime("/r/unsummarized.txt", EntryKind::File, 100),
            dummy_entry("/r/no-mtime.txt", EntryKind::File, 1),
        ])
        .unwrap();
    store
        .db_connection()
        .execute_batch("UPDATE entries SET deep_policy = 'Skip' WHERE path = '/r/skipped.txt'")
        .unwrap();
    for (path, generated_at) in [
        ("/r", 200i64),          // dir summary newer than dir mtime → fresh
        ("/r/stale.txt", 50),    // summary predates the edit → stale
        ("/r/fresh.txt", 200),   // summary postdates the mtime → fresh
        ("/r/skipped.txt", 50),  // stale by time, but deep_policy = Skip
        ("/r/no-mtime.txt", 50), // NULL mtime → unknowable, not flagged
    ] {
        let mut rec = dummy_summary(path, "file", Some("/r"), 2);
        rec.generated_at = generated_at;
        store.upsert_summary(&rec).unwrap();
    }

    let stale = store.stale_summary_candidates("/r").unwrap();
    assert_eq!(stale, vec![("/r/stale.txt".to_owned(), "file".to_owned())]);
}

#[test]
fn mark_for_resummary_batch_resets_done_but_not_in_flight() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .enqueue_summary_items(&[
            ("/q/done.txt".into(), "file".into(), 2),
            ("/q/busy.txt".into(), "file".into(), 2),
        ])
        .unwrap();
    store.mark_queue_state("/q/done.txt", "done", None).unwrap();
    store
        .mark_queue_state("/q/busy.txt", "in_flight", None)
        .unwrap();

    store
        .mark_for_resummary_batch(&[
            ("/q/done.txt".into(), "file".into(), 2),
            ("/q/busy.txt".into(), "file".into(), 2),
            ("/q/new.txt".into(), "file".into(), 2),
        ])
        .unwrap();

    assert_eq!(
        store.queue_state("/q/done.txt").unwrap().as_deref(),
        Some("pending")
    );
    // An in_flight row is a worker's active claim — the batch must not steal it.
    assert_eq!(
        store.queue_state("/q/busy.txt").unwrap().as_deref(),
        Some("in_flight")
    );
    assert_eq!(
        store.queue_state("/q/new.txt").unwrap().as_deref(),
        Some("pending")
    );
    assert_eq!(store.queue_state("/q/absent.txt").unwrap(), None);
}

#[test]
fn symbol_pin_narrows_who_calls_and_blast_radius() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            // Two definitions of `parse`; one caller imports the /lib one, one
            // caller is unrelated (bare).
            edge("/proj/lib/parser.py", "defines", "parse"),
            edge("/other/tool.py", "defines", "parse"),
            edge("/proj/app/main.py", "calls", "parse"),
            edge("/proj/app/main.py", "imports", "lib.parser"),
            edge("/misc/script.py", "calls", "parse"),
        ])
        .unwrap();

    // Unpinned: both callers are in the blast radius.
    let br = store.blast_radius_resolved("parse", 100, false, 2).unwrap();
    assert_eq!(br.direct, 2);

    // The user pins /other/tool.py as authoritative via the ledger.
    let id = store
        .record_decision(NewDecision {
            decision_type: "symbol_ambiguity".into(),
            subject: "parse".into(),
            params: serde_json::json!({"definers": ["/other/tool.py", "/proj/lib/parser.py"]}),
            options: serde_json::json!(["/other/tool.py", "/proj/lib/parser.py", "all"]),
            auto_value: None,
            confidence: None,
            evidence_hash: "fp".into(),
            priority: 20,
            paths: vec![],
        })
        .unwrap()
        .unwrap();
    store.answer_decision(id, "/other/tool.py", "user").unwrap();

    // Pinned: main.py's call import-resolves to /proj/lib (NOT the pin) → dropped;
    // the bare caller stays (no evidence either way) in non-strict mode.
    let br = store.blast_radius_resolved("parse", 100, false, 2).unwrap();
    assert_eq!(br.direct, 1, "import-resolved-elsewhere caller must drop");
    assert!(br.files.contains(&"/misc/script.py".to_owned()));
    // Strict also drops the bare caller.
    let br = store.blast_radius_resolved("parse", 100, true, 2).unwrap();
    assert_eq!(br.direct, 0);

    // who_calls resolves against the pinned definer only.
    let callers = store.who_calls_resolved("parse", 100).unwrap();
    for c in &callers {
        if c.tier != ResolutionTier::Bare {
            assert_eq!(c.targets, vec!["/other/tool.py".to_owned()]);
        }
    }
}

#[test]
fn entry_by_path_returns_facts_or_none() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/r/a.rs", EntryKind::File, 42)])
        .unwrap();
    let e = store.entry_by_path("/r/a.rs").unwrap().unwrap();
    assert_eq!(e.kind, "file");
    assert_eq!(e.size, 42);
    assert!(store.entry_by_path("/r/missing.rs").unwrap().is_none());
}

#[test]
fn boost_with_recency_ranks_fresh_above_stale() {
    use std::time::{Duration, SystemTime};
    let mut store = Store::open_in_memory().unwrap();
    let now = SystemTime::now();
    let fresh = Entry {
        path: PathBuf::from("/proj/fresh.rs"),
        kind: EntryKind::File,
        size: 10,
        modified: Some(now),
        hint: None,
        is_binary: false,
    };
    let stale = Entry {
        path: PathBuf::from("/proj/stale.rs"),
        kind: EntryKind::File,
        size: 10,
        modified: Some(now - Duration::from_secs(200 * 86_400)),
        hint: None,
        is_binary: false,
    };
    store.upsert_entries(&[fresh, stale]).unwrap();

    let mk = |path: &str| SearchHit {
        chunk_id: 0,
        entry_path: path.to_owned(),
        seq: 0,
        heading: String::new(),
        text: "x".to_owned(),
        rrf_score: 1.0,
    };
    let mut hits = vec![mk("/proj/stale.rs"), mk("/proj/fresh.rs")];
    store.boost_with_recency(&mut hits, 90).unwrap();

    let fresh_score = hits
        .iter()
        .find(|h| h.entry_path.ends_with("fresh.rs"))
        .unwrap()
        .rrf_score;
    let stale_score = hits
        .iter()
        .find(|h| h.entry_path.ends_with("stale.rs"))
        .unwrap()
        .rrf_score;
    // today → ×2.0; 200 days → outside the 90-day window → ×1.0 (neutral).
    assert!(
        fresh_score > stale_score,
        "fresh {fresh_score} should outrank stale {stale_score}"
    );
    assert!((fresh_score - 2.0).abs() < 1e-9, "fresh got {fresh_score}");
    assert!((stale_score - 1.0).abs() < 1e-9, "stale got {stale_score}");
}
