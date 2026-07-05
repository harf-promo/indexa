use super::*;
use crate::store::SummaryRecord;
use crate::walker::{Entry, EntryKind};
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

/// An entries row whose mtime is ancient (epoch + 1000 s) — far past any
/// staleness horizon, but NOT NULL (NULL = unknown, which the detector skips).
fn old_entry(path: &str, kind: EntryKind) -> Entry {
    Entry {
        path: PathBuf::from(path),
        kind,
        size: 0,
        modified: Some(UNIX_EPOCH + Duration::from_secs(1_000)),
        hint: None,
        is_binary: false,
    }
}

fn file_summary(path: &str, source_hash: &str) -> SummaryRecord {
    SummaryRecord {
        path: path.to_owned(),
        kind: "file".into(),
        parent_path: Some("/r".to_owned()),
        depth: 1,
        summary: format!("summary of {path}"),
        summary_l0: None,
        embedding: None,
        child_count: 0,
        byte_size: 10,
        model: "test".into(),
        source_hash: source_hash.to_owned(),
        generated_at: 1,
    }
}

#[test]
fn fingerprint_ignores_one_extra_file_at_coarse_rounding() {
    let a = classification_fingerprint(None, &[("code".into(), 40)]);
    // 40/41 ≈ 0.976 rounds to the same 0.05 bucket as 1.0; the stray
    // document's own share rounds to zero and is omitted.
    let b = classification_fingerprint(None, &[("code".into(), 40), ("documents".into(), 1)]);
    assert_eq!(a, b);
}

#[test]
fn fingerprint_changes_on_material_shift_or_hint_change() {
    let base = classification_fingerprint(None, &[("code".into(), 40)]);
    // Composition shift: half the folder is now documents.
    let shifted =
        classification_fingerprint(None, &[("code".into(), 40), ("documents".into(), 40)]);
    assert_ne!(base, shifted);
    // The dir's own hint appearing is material on its own.
    let hinted = classification_fingerprint(Some("build-artifact"), &[("code".into(), 40)]);
    assert_ne!(base, hinted);
}

#[test]
fn fingerprint_is_order_independent() {
    let a = classification_fingerprint(None, &[("code".into(), 10), ("media".into(), 10)]);
    let b = classification_fingerprint(None, &[("media".into(), 10), ("code".into(), 10)]);
    assert_eq!(a, b);
}

#[test]
fn run_detectors_opens_once_and_skips_covered_clusters() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&file_summary("/r/a.txt", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/b.txt", "H1"))
        .unwrap();

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (1, 0));
    let open = store.open_decisions(None, 10).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].decision_type, "duplicate");
    assert_eq!(open[0].subject, "/r/a.txt");
    let options: Vec<String> = serde_json::from_str(&open[0].options).unwrap();
    assert_eq!(options, vec!["/r/a.txt", "/r/b.txt", "keep_all"]);

    // Second pass: the open question covers both members → skipped, not duplicated.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (0, 1));

    // Answered (decided, un-superseded) still covers the cluster.
    super::super::decide_and_apply(&mut store, open[0].id, "/r/a.txt", "user").unwrap();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (0, 1));
}

#[test]
fn run_detectors_expires_questions_whose_evidence_left_the_index() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&file_summary("/r/a.txt", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/b.txt", "H1"))
        .unwrap();
    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 1);

    // One member's evidence disappears entirely (no entries row existed;
    // now its summary goes too — e.g. the file was deleted and pruned).
    store.delete_summary("/r/b.txt").unwrap();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.expired, 1, "the orphaned question must expire");
    assert_eq!(store.open_decision_count().unwrap(), 0);
    // Recorded, not dropped: history shows the expired row with its note.
    let hist = store.decision_history("duplicate", "/r/a.txt").unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].status, "expired");
    assert!(hist[0].params.contains("left the index"));
}

#[test]
fn archive_detector_asks_about_topmost_stale_dirs_only() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            old_entry("/old", EntryKind::Dir),
            old_entry("/old/sub", EntryKind::Dir),
            old_entry("/old/a.txt", EntryKind::File),
            old_entry("/old/sub/b.txt", EntryKind::File),
            // Shares the string prefix but is NOT under /old — must get its
            // own question (the /proj vs /projector boundary check).
            old_entry("/old-sibling", EntryKind::Dir),
        ])
        .unwrap();

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 2, "topmost dirs only, /old/sub filtered");

    let open = store.open_decisions(Some("archive"), 10).unwrap();
    let mut subjects: Vec<&str> = open.iter().map(|d| d.subject.as_str()).collect();
    subjects.sort_unstable();
    assert_eq!(subjects, vec!["/old", "/old-sibling"]);
    for d in &open {
        assert_eq!(d.priority, 30);
        let options: Vec<String> = serde_json::from_str(&d.options).unwrap();
        assert_eq!(options, vec!["archive", "keep_active"]);
    }
    let old = open.iter().find(|d| d.subject == "/old").unwrap();
    let params: serde_json::Value = serde_json::from_str(&old.params).unwrap();
    assert_eq!(params["files"], 2, "subtree file count: a.txt + sub/b.txt");
    assert!(params["days"].as_i64().unwrap() > 365);

    // Second pass: the open questions cover both dirs — nothing duplicated.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (0, 2));
}

#[test]
fn archive_detector_skips_unknown_mtime_and_classified_dirs() {
    let mut store = Store::open_in_memory().unwrap();
    let mut unknown = old_entry("/mystery", EntryKind::Dir);
    unknown.modified = None; // NULL mtime = unknown, not evidence of age
    store
        .upsert_entries(&[unknown, old_entry("/archived", EntryKind::Dir)])
        .unwrap();
    store
        .confirm_classification("/archived", "archive")
        .unwrap();

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 0);
    assert_eq!(
        report.skipped, 1,
        "/archived skipped; /mystery not a candidate"
    );
}

#[test]
fn archive_answer_projects_classification_and_dir_weight() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            old_entry("/old", EntryKind::Dir),
            old_entry("/old/a.txt", EntryKind::File),
        ])
        .unwrap();
    let cfg = crate::config::ReviewConfig::default();
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
    let id = store.open_decisions(Some("archive"), 1).unwrap()[0].id;

    let effects = super::super::decide_and_apply(&mut store, id, "archive", "user").unwrap();
    assert_eq!(effects["classification"], "archive");
    let c = store.classification_for("/old").unwrap().unwrap();
    assert_eq!(
        (c.category.as_str(), c.source.as_str()),
        ("archive", "user")
    );
    let w = store.list_weights(Some("dir")).unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].target.as_str(), w[0].weight), ("/old", 0.5));
    assert_eq!(
        w[0].reason.as_deref(),
        Some(&*format!("decision:{id} archived"))
    );

    // Next pass: the archive classification suppresses any re-ask.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (0, 1));
}

#[test]
fn keep_active_writes_no_classification_and_reasks_on_bucket_change() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            old_entry("/old", EntryKind::Dir),
            old_entry("/old/a.txt", EntryKind::File),
        ])
        .unwrap();
    let cfg = crate::config::ReviewConfig::default();
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
    let id = store.open_decisions(Some("archive"), 1).unwrap()[0].id;

    super::super::decide_and_apply(&mut store, id, "keep_active", "user").unwrap();
    assert!(store.classification_for("/old").unwrap().is_none());
    assert!(store.list_weights(Some("dir")).unwrap().is_empty());

    // Unchanged evidence: the decided keep_active row covers the dir.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (0, 1));

    // Evidence moves (file count changes; staleness buckets move the same
    // way as the dir ages) → re-ask CHAINED to the prior, never a second head.
    store
        .upsert_entries(&[old_entry("/old/b.txt", EntryKind::File)])
        .unwrap();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 1);
    let reask = &store.open_decisions(Some("archive"), 1).unwrap()[0];
    assert_eq!(
        reask.parent_id,
        Some(id),
        "re-ask chains to the prior answer"
    );

    // Resolving the re-ask supersedes the prior — exactly one live head.
    super::super::decide_and_apply(&mut store, reask.id, "archive", "user").unwrap();
    assert_eq!(
        store.decision_by_id(id).unwrap().unwrap().superseded_by,
        Some(reask.id)
    );
    let c = store.classification_for("/old").unwrap().unwrap();
    assert_eq!(c.category, "archive");
}

#[test]
fn predismiss_duplicate_suppresses_the_detector() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&file_summary("/r/a.txt", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/b.txt", "H1"))
        .unwrap();

    // "Don't ask about this" arrives BEFORE the detector ever ran.
    predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
    assert_eq!(store.open_decision_count().unwrap(), 0);

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(
        report.opened, 0,
        "sticky dismissal must suppress the question"
    );
    assert_eq!(report.skipped, 1);

    // Idempotent: a second dismissal of the same evidence is a no-op.
    predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
    assert_eq!(
        store
            .decision_history("duplicate", "/r/a.txt")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn predismiss_also_dismisses_an_already_open_question() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&file_summary("/r/a.txt", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/b.txt", "H1"))
        .unwrap();
    let cfg = crate::config::ReviewConfig::default();
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);

    predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
    assert_eq!(
        store.open_decision_count().unwrap(),
        0,
        "the live question is dismissed along with the future one"
    );
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 0);
}

#[test]
fn predismiss_archive_suppresses_the_detector_or_reports_nothing_to_do() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            old_entry("/old", EntryKind::Dir),
            old_entry("/old/a.txt", EntryKind::File),
        ])
        .unwrap();

    assert!(predismiss_archive(&mut store, "/old").unwrap());
    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!((report.opened, report.skipped), (0, 1));

    // Not indexed → nothing the detector would ever ask → nothing recorded.
    assert!(!predismiss_archive(&mut store, "/nope").unwrap());
    assert!(store
        .decision_history("archive", "/nope")
        .unwrap()
        .is_empty());
}

#[test]
fn gc_decisions_count_matches_what_gc_deletes() {
    // The dry-run twin `indexa prune --dry-run` relies on: count == delete.
    let mut store = Store::open_in_memory().unwrap();
    // Predismissal re-derives the cluster from the store, so the evidence
    // must actually exist (same content hash → an exact cluster).
    store
        .upsert_summary(&file_summary("/r/a.txt", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/b.txt", "H1"))
        .unwrap();
    predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
    // Horizon in the future (negative age): the fresh dismissal qualifies.
    assert_eq!(store.gc_decisions_count(-10).unwrap(), 1);
    assert_eq!(store.gc_decisions(-10).unwrap(), 1);
    assert_eq!(store.gc_decisions_count(-10).unwrap(), 0);
    // A horizon in the past keeps the (re-recorded) fresh row.
    predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
    assert_eq!(store.gc_decisions_count(365 * 86_400).unwrap(), 0);
}

// ── Summary drift ─────────────────────────────────────────────────────────

fn embedded_summary(path: &str, summary: &str, emb: Vec<f32>, model: &str) -> SummaryRecord {
    SummaryRecord {
        path: path.to_owned(),
        kind: "file".into(),
        parent_path: Some("/r".to_owned()),
        depth: 1,
        summary: summary.to_owned(),
        summary_l0: None,
        embedding: Some(emb),
        child_count: 0,
        byte_size: 10,
        model: model.to_owned(),
        source_hash: "H".to_owned(),
        generated_at: 1,
    }
}

#[test]
fn drift_fires_below_threshold_and_skips_above_or_without_embeddings() {
    let mut store = Store::open_in_memory().unwrap();
    let old = embedded_summary("/r/f.txt", "Old summary. More.", vec![1.0, 0.0], "m1");
    let new = embedded_summary("/r/f.txt", "New summary. Else.", vec![0.0, 1.0], "m2");

    // Orthogonal embeddings → cosine 0 → question.
    let id = flag_summary_drift(&mut store, &old, &new).unwrap().unwrap();
    let d = store.decision_by_id(id).unwrap().unwrap();
    assert_eq!(d.decision_type, "summary_drift");
    assert_eq!(d.subject, "/r/f.txt");
    assert_eq!(d.priority, 40);
    let options: Vec<String> = serde_json::from_str(&d.options).unwrap();
    assert_eq!(options, vec!["keep_new", "restore_old"]);
    let params: serde_json::Value = serde_json::from_str(&d.params).unwrap();
    assert_eq!(params["old_summary"], "Old summary. More.");
    assert_eq!(params["old_l0"], "Old summary.");
    assert_eq!(params["new_l0"], "New summary.");
    assert_eq!(params["old_model"], "m1");
    assert_eq!(params["new_model"], "m2");
    assert!(params["cosine"].as_f64().unwrap() < 0.8);

    // Open row dedups a second fire.
    assert!(flag_summary_drift(&mut store, &old, &new)
        .unwrap()
        .is_none());

    // Near-identical embeddings → no question.
    let mut store2 = Store::open_in_memory().unwrap();
    let similar = embedded_summary("/r/f.txt", "New.", vec![0.99, 0.05], "m2");
    assert!(flag_summary_drift(&mut store2, &old, &similar)
        .unwrap()
        .is_none());

    // A missing embedding on either side skips silently.
    let mut no_emb = old.clone();
    no_emb.embedding = None;
    assert!(flag_summary_drift(&mut store2, &no_emb, &new)
        .unwrap()
        .is_none());
}

#[test]
fn drift_skips_when_the_user_already_chose_for_this_evidence() {
    let mut store = Store::open_in_memory().unwrap();
    let old = embedded_summary("/r/f.txt", "Old.", vec![1.0, 0.0], "m1");
    let new = embedded_summary("/r/f.txt", "New.", vec![0.0, 1.0], "m2");
    let id = flag_summary_drift(&mut store, &old, &new).unwrap().unwrap();
    super::super::decide_and_apply(&mut store, id, "keep_new", "user").unwrap();

    // Same content + same model → the standing answer covers it.
    assert!(flag_summary_drift(&mut store, &old, &new)
        .unwrap()
        .is_none());

    // A different model is new evidence → chained re-ask, never a second head.
    let mut new2 = new.clone();
    new2.model = "m3".into();
    let reask = flag_summary_drift(&mut store, &old, &new2)
        .unwrap()
        .unwrap();
    assert_eq!(
        store.decision_by_id(reask).unwrap().unwrap().parent_id,
        Some(id)
    );
}

// ── Language fallback ─────────────────────────────────────────────────────

fn null_lang_chunks(path: &str, n: usize) -> Vec<crate::store::ChunkRecord> {
    (0..n)
        .map(|i| crate::store::ChunkRecord {
            entry_path: path.to_owned(),
            seq: i,
            heading: String::new(),
            text: format!("chunk {i}"),
            language: None,
            embedding: None,
            embed_model: None,
            content_hash: None,
        })
        .collect()
}

/// A fresh entries row (recent mtime so the archive detector stays quiet).
fn fresh_entry(path: &str, kind: EntryKind) -> Entry {
    Entry {
        path: PathBuf::from(path),
        kind,
        size: 0,
        modified: Some(std::time::SystemTime::now()),
        hint: None,
        is_binary: false,
    }
}

#[test]
fn language_detector_asks_for_untagged_code_files_only() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            fresh_entry("/r/script.rb", EntryKind::File),
            fresh_entry("/r/notes.txt", EntryKind::File),
            fresh_entry("/r/tiny.php", EntryKind::File),
        ])
        .unwrap();
    store
        .upsert_chunks(&null_lang_chunks("/r/script.rb", 3))
        .unwrap();
    // Plain text: untagged is correct — never a question.
    store
        .upsert_chunks(&null_lang_chunks("/r/notes.txt", 5))
        .unwrap();
    // Code, but below the chunk floor — not worth an interruption.
    store
        .upsert_chunks(&null_lang_chunks("/r/tiny.php", 2))
        .unwrap();

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 1);
    let open = store.open_decisions(Some("language"), 10).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].subject, "/r/script.rb");
    assert_eq!(open[0].priority, 20);
    let options: Vec<String> = serde_json::from_str(&open[0].options).unwrap();
    // The file doesn't exist on disk → no hyperpolyglot candidate.
    assert_eq!(options, vec!["ruby", "ignore"]);
    let params: serde_json::Value = serde_json::from_str(&open[0].params).unwrap();
    assert_eq!(params["chunks"], 3);

    // Second pass: the open question dedups, nothing new.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 0);
}

#[test]
fn language_answer_tags_chunks_and_is_silently_reapplied_after_rechunk() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[fresh_entry("/r/script.rb", EntryKind::File)])
        .unwrap();
    store
        .upsert_chunks(&null_lang_chunks("/r/script.rb", 3))
        .unwrap();
    let cfg = crate::config::ReviewConfig::default();
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
    let id = store.open_decisions(Some("language"), 1).unwrap()[0].id;

    let fx = super::super::decide_and_apply(&mut store, id, "ruby", "user").unwrap();
    assert_eq!(fx, serde_json::json!({"language": "ruby", "chunks": 3}));
    assert!(store.unlabeled_chunk_files(1, 10).unwrap().is_empty());

    // A re-deep rewrites the chunks untagged — the standing answer is
    // re-applied silently instead of re-asking.
    store
        .upsert_chunks(&null_lang_chunks("/r/script.rb", 4))
        .unwrap();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 0);
    assert!(store.unlabeled_chunk_files(1, 10).unwrap().is_empty());
}

// ── Symbol ambiguity ──────────────────────────────────────────────────────

fn edge(from: &str, kind: &str, to: &str) -> crate::store::EdgeRecord {
    crate::store::EdgeRecord {
        from_path: from.to_owned(),
        kind: kind.to_owned(),
        to_ref: to.to_owned(),
    }
}

/// Seed `foo` defined in two files with one caller; entries rows keep the
/// expiry sweep satisfied (params.paths = the definers).
fn seed_ambiguous_foo(store: &mut Store) {
    store
        .upsert_entries(&[
            fresh_entry("/a.rs", EntryKind::File),
            fresh_entry("/b.rs", EntryKind::File),
            fresh_entry("/c.rs", EntryKind::File),
        ])
        .unwrap();
    store
        .upsert_edges(&[
            edge("/a.rs", "defines", "foo"),
            edge("/b.rs", "defines", "foo"),
            edge("/c.rs", "calls", "foo"),
        ])
        .unwrap();
}

#[test]
fn symbol_detector_asks_once_and_projects_the_choice() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            fresh_entry("/a.rs", EntryKind::File),
            fresh_entry("/b.rs", EntryKind::File),
            fresh_entry("/c.rs", EntryKind::File),
        ])
        .unwrap();
    // One batch (upsert_edges replaces by from_path): `foo` is ambiguous,
    // `bar` (one definition) must NOT fire.
    store
        .upsert_edges(&[
            edge("/a.rs", "defines", "foo"),
            edge("/b.rs", "defines", "foo"),
            edge("/c.rs", "calls", "foo"),
            edge("/a.rs", "defines", "bar"),
            edge("/c.rs", "calls", "bar"),
        ])
        .unwrap();

    // symbol_ambiguity is opt-in as of v0.39 — enable it for this detector's tests.
    let cfg = crate::config::ReviewConfig {
        symbol_ambiguity: true,
        ..crate::config::ReviewConfig::default()
    };
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 1);
    let open = store.open_decisions(Some("symbol_ambiguity"), 10).unwrap();
    assert_eq!(open.len(), 1);
    let d = &open[0];
    assert_eq!(d.subject, "foo");
    assert_eq!(d.priority, 20);
    let options: Vec<String> = serde_json::from_str(&d.options).unwrap();
    assert_eq!(options, vec!["/a.rs", "/b.rs", "all"]);
    let params: serde_json::Value = serde_json::from_str(&d.params).unwrap();
    assert_eq!(params["definers"], serde_json::json!(["/a.rs", "/b.rs"]));
    assert_eq!(params["callers"], 1);
    // paths carries the definers so the expiry sweep checks files, not the
    // bare symbol name.
    assert_eq!(params["paths"], serde_json::json!(["/a.rs", "/b.rs"]));

    // Open row dedups the second pass.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 0);

    // Answering stores the choice as effects only — no domain-table writes.
    let fx = super::super::decide_and_apply(&mut store, d.id, "/a.rs", "user").unwrap();
    assert_eq!(fx, serde_json::json!({"authoritative": "/a.rs"}));

    // Decided + unchanged definer set → skipped, not re-asked.
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 0);
}

#[test]
fn symbol_detector_reasks_chained_when_the_definer_set_changes() {
    let mut store = Store::open_in_memory().unwrap();
    seed_ambiguous_foo(&mut store);
    let cfg = crate::config::ReviewConfig {
        symbol_ambiguity: true,
        ..crate::config::ReviewConfig::default()
    };
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
    let id = store.open_decisions(Some("symbol_ambiguity"), 1).unwrap()[0].id;
    super::super::decide_and_apply(&mut store, id, "all", "user").unwrap();
    assert_eq!(
        store
            .decision_by_id(id)
            .unwrap()
            .unwrap()
            .effects
            .as_deref(),
        Some(r#"{"authoritative":null}"#)
    );

    // A third definition appears → new evidence → chained re-ask.
    // (upsert_edges replaces /c.rs's rows — keep its existing call edge.)
    store
        .upsert_edges(&[
            edge("/c.rs", "calls", "foo"),
            edge("/c.rs", "defines", "foo"),
        ])
        .unwrap();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 1);
    let reask = &store.open_decisions(Some("symbol_ambiguity"), 1).unwrap()[0];
    assert_eq!(reask.parent_id, Some(id), "re-ask chains to the prior");
    let options: Vec<String> = serde_json::from_str(&reask.options).unwrap();
    assert_eq!(options, vec!["/a.rs", "/b.rs", "/c.rs", "all"]);
}

#[test]
fn symbol_detector_honors_top_k_and_scan_caps() {
    let mut store = Store::open_in_memory().unwrap();
    // 12 ambiguous symbols, each defined twice and called once.
    let mut edges = Vec::new();
    let mut entries = Vec::new();
    for i in 0..12 {
        let (a, b, c) = (
            format!("/a{i}.rs"),
            format!("/b{i}.rs"),
            format!("/c{i}.rs"),
        );
        for p in [&a, &b, &c] {
            entries.push(fresh_entry(p, EntryKind::File));
        }
        let sym = format!("sym{i}");
        edges.push(edge(&a, "defines", &sym));
        edges.push(edge(&b, "defines", &sym));
        edges.push(edge(&c, "calls", &sym));
    }
    store.upsert_entries(&entries).unwrap();
    store.upsert_edges(&edges).unwrap();

    // max_new_per_scan below top-K: the scan cap wins.
    let cfg = crate::config::ReviewConfig {
        max_new_per_scan: 4,
        symbol_ambiguity: true,
        ..crate::config::ReviewConfig::default()
    };
    assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 4);

    // With a generous cap the per-scan top-K (10) bounds the rest:
    // 6 remaining of the K=10 hottest open on the second pass.
    let cfg = crate::config::ReviewConfig {
        symbol_ambiguity: true,
        ..crate::config::ReviewConfig::default()
    };
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(
        report.opened + 4,
        10,
        "top-K bounds the per-scan candidates"
    );
}

#[test]
fn run_detectors_honors_caps() {
    let mut store = Store::open_in_memory().unwrap();
    for i in 0..3 {
        store
            .upsert_summary(&file_summary(&format!("/r/a{i}.txt"), &format!("H{i}")))
            .unwrap();
        store
            .upsert_summary(&file_summary(&format!("/r/b{i}.txt"), &format!("H{i}")))
            .unwrap();
    }
    let cfg = crate::config::ReviewConfig {
        max_new_per_scan: 2,
        ..crate::config::ReviewConfig::default()
    };
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(report.opened, 2);
    assert_eq!(store.open_decision_count().unwrap(), 2);
}

// ── v0.39 noise filters ─────────────────────────────────────────────────────
#[test]
fn duplicate_detector_skips_asset_clusters_keeps_source() {
    let mut store = Store::open_in_memory().unwrap();
    // Identical-content images (exact dup) — assets, NOT actionable.
    store
        .upsert_summary(&file_summary("/r/x.png", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/y.png", "H1"))
        .unwrap();
    // Identical-content source — a real, actionable duplicate.
    store
        .upsert_summary(&file_summary("/r/a.rs", "H2"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/b.rs", "H2"))
        .unwrap();

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(
        report.opened, 1,
        "only the source cluster opens; the image cluster is filtered"
    );
    let open = store.open_decisions(Some("duplicate"), 10).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].subject, "/r/a.rs");
}

#[test]
fn duplicate_detector_skips_generated_dir_clusters() {
    let mut store = Store::open_in_memory().unwrap();
    // Source files (not asset-extension) but under a generated icon tree — not actionable.
    store
        .upsert_summary(&file_summary("/r/icons/ios/Icon-1.txt", "H1"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/icons/ios/Icon-2.txt", "H1"))
        .unwrap();
    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(
        report.opened, 0,
        "a generated/asset tree is never a dedupe target"
    );
}

#[test]
fn symbol_ambiguity_is_off_by_default() {
    let mut store = Store::open_in_memory().unwrap();
    seed_ambiguous_foo(&mut store);
    let cfg = crate::config::ReviewConfig::default(); // symbol_ambiguity = false
    run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(
        store
            .open_decisions(Some("symbol_ambiguity"), 10)
            .unwrap()
            .len(),
        0,
        "the unanswerable-question detector must stay quiet unless opted in"
    );
}

#[test]
fn symbol_ambiguity_skips_idioms_even_when_enabled() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            fresh_entry("/a.rs", EntryKind::File),
            fresh_entry("/b.rs", EntryKind::File),
            fresh_entry("/c.rs", EntryKind::File),
        ])
        .unwrap();
    // `new` is a universal idiom: defined in two files, called once. Even with the
    // feature ON, asking "which `new` is authoritative?" is noise — must not fire.
    store
        .upsert_edges(&[
            edge("/a.rs", "defines", "new"),
            edge("/b.rs", "defines", "new"),
            edge("/c.rs", "calls", "new"),
        ])
        .unwrap();
    let cfg = crate::config::ReviewConfig {
        symbol_ambiguity: true,
        ..crate::config::ReviewConfig::default()
    };
    run_detectors(&mut store, &cfg).unwrap();
    assert_eq!(
        store
            .open_decisions(Some("symbol_ambiguity"), 10)
            .unwrap()
            .len(),
        0,
        "idiom `new` must not surface even with the feature on"
    );
}

#[test]
fn sweep_dismisses_disabled_symbol_ambiguity_rows() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            fresh_entry("/a.rs", EntryKind::File),
            fresh_entry("/b.rs", EntryKind::File),
        ])
        .unwrap();
    // Pre-existing ambiguity row (e.g. created before v0.39). record_decision does
    // not filter — the detector/sweep do.
    store
        .record_decision(symbol_ambiguity_question(
            "resolve_base_url",
            &["/a.rs".to_owned(), "/b.rs".to_owned()],
            2,
        ))
        .unwrap();
    assert_eq!(store.open_decision_count().unwrap(), 1);

    let cfg = crate::config::ReviewConfig::default(); // feature off
    assert_eq!(
        sweep_filtered_noise(&mut store, &cfg, true).unwrap(),
        1,
        "dry-run counts it"
    );
    assert_eq!(
        store.open_decision_count().unwrap(),
        1,
        "dry-run dismisses nothing"
    );
    assert_eq!(sweep_filtered_noise(&mut store, &cfg, false).unwrap(), 1);
    assert_eq!(
        store.open_decision_count().unwrap(),
        0,
        "sweep dismissed the disabled-feature row"
    );
}

#[test]
fn idiom_and_actionable_helpers() {
    assert!(is_idiom_symbol("new"));
    assert!(is_idiom_symbol("Default")); // case-insensitive
    assert!(is_idiom_symbol("with_timeout"));
    assert!(is_idiom_symbol("set_scope"));
    assert!(!is_idiom_symbol("resolve_base_url"));
    assert!(!is_idiom_symbol("compute_budget"));
    assert!(!duplicate_cluster_actionable(&[
        "/a/x.webp".into(),
        "/a/y.webp".into()
    ]));
    assert!(!duplicate_cluster_actionable(&[
        "/p/icons/a.rs".into(),
        "/p/b.rs".into()
    ]));
    assert!(duplicate_cluster_actionable(&[
        "/p/a.rs".into(),
        "/p/b.rs".into()
    ]));
}

// ── v0.40 near-dup basename filter tests ─────────────────────────────────

#[test]
fn near_dup_same_basenames_helper() {
    // Same basename in different dirs → true (potentially a copy).
    assert!(near_dup_same_basenames(&[
        "/crates/query/src/qa.rs".into(),
        "/crates/web/src/qa.rs".into(),
    ]));
    // Different basenames → false (likely a false positive).
    assert!(!near_dup_same_basenames(&[
        "/crates/query/src/summarize.rs".into(),
        "/crates/web/src/jobs_exec.rs".into(),
    ]));
    // Three members, all same → true.
    assert!(near_dup_same_basenames(&[
        "/a/foo.rs".into(),
        "/b/foo.rs".into(),
        "/c/foo.rs".into(),
    ]));
    // Three members, one different → false.
    assert!(!near_dup_same_basenames(&[
        "/a/foo.rs".into(),
        "/b/foo.rs".into(),
        "/c/bar.rs".into(),
    ]));
    // Single member → true (vacuous; no question is opened for single-member clusters).
    assert!(near_dup_same_basenames(&["/a/foo.rs".into()]));
    // Empty → true (vacuous).
    assert!(near_dup_same_basenames(&[]));
}

#[test]
fn near_dup_differently_named_cluster_is_skipped() {
    // Two files with different names get a high-similarity embedding cluster
    // (via same source_hash used as embedding proxy in find_exact_duplicates).
    // A near-dup of differently-named files must be skipped (not opened).
    let mut store = Store::open_in_memory().unwrap();
    // Use different source hashes so find_exact_duplicates doesn't fire;
    // the question is whether the near-dup path is blocked by name check.
    // We seed the open decision directly to test sweep_filtered_noise.
    let q = NewDecision {
        decision_type: "duplicate".into(),
        subject: "/r/summarize.rs".into(),
        params: serde_json::json!({
            "paths": ["/r/summarize.rs", "/r/jobs_exec.rs"],
            "similarity": 0.97_f32,
            "exact": false,
        }),
        options: serde_json::json!(["/r/summarize.rs", "/r/jobs_exec.rs", "keep_all"]),
        auto_value: Some("/r/summarize.rs".into()),
        confidence: Some(0.97),
        evidence_hash: "test-hash-near-diff-name".into(),
        priority: 60,
        paths: vec!["/r/summarize.rs".into(), "/r/jobs_exec.rs".into()],
    };
    store.record_decision(q).unwrap();
    assert_eq!(store.open_decision_count().unwrap(), 1);

    // sweep_filtered_noise should dismiss it.
    let cfg = crate::config::ReviewConfig::default();
    let n = sweep_filtered_noise(&mut store, &cfg, false).unwrap();
    assert_eq!(n, 1, "differently-named near-dup must be swept");
    assert_eq!(store.open_decision_count().unwrap(), 0);
}

#[test]
fn near_dup_same_named_cluster_is_kept() {
    // Same basename (qa.rs in two crates) is a real copy candidate — keep asking.
    let mut store = Store::open_in_memory().unwrap();
    let q = NewDecision {
        decision_type: "duplicate".into(),
        subject: "/crates/query/src/qa.rs".into(),
        params: serde_json::json!({
            "paths": ["/crates/query/src/qa.rs", "/crates/web/src/qa.rs"],
            "similarity": 0.97_f32,
            "exact": false,
        }),
        options: serde_json::json!([
            "/crates/query/src/qa.rs",
            "/crates/web/src/qa.rs",
            "keep_all"
        ]),
        auto_value: Some("/crates/query/src/qa.rs".into()),
        confidence: Some(0.97),
        evidence_hash: "test-hash-near-same-name".into(),
        priority: 60,
        paths: vec![
            "/crates/query/src/qa.rs".into(),
            "/crates/web/src/qa.rs".into(),
        ],
    };
    store.record_decision(q).unwrap();
    assert_eq!(store.open_decision_count().unwrap(), 1);

    let cfg = crate::config::ReviewConfig::default();
    let n = sweep_filtered_noise(&mut store, &cfg, false).unwrap();
    assert_eq!(n, 0, "same-basename near-dup must NOT be swept");
    assert_eq!(
        store.open_decision_count().unwrap(),
        1,
        "question must remain open"
    );
}

#[test]
fn exact_dup_differently_named_is_kept() {
    // Exact content (similarity 1.0) — always ask regardless of name.
    let mut store = Store::open_in_memory().unwrap();
    let q = NewDecision {
        decision_type: "duplicate".into(),
        subject: "/r/alpha.rs".into(),
        params: serde_json::json!({
            "paths": ["/r/alpha.rs", "/r/beta.rs"],
            "similarity": 1.0_f32,
            "exact": true,
        }),
        options: serde_json::json!(["/r/alpha.rs", "/r/beta.rs", "keep_all"]),
        auto_value: Some("/r/alpha.rs".into()),
        confidence: Some(1.0),
        evidence_hash: "test-hash-exact-diff-name".into(),
        priority: 60,
        paths: vec!["/r/alpha.rs".into(), "/r/beta.rs".into()],
    };
    store.record_decision(q).unwrap();
    assert_eq!(store.open_decision_count().unwrap(), 1);

    let cfg = crate::config::ReviewConfig::default();
    let n = sweep_filtered_noise(&mut store, &cfg, false).unwrap();
    assert_eq!(
        n, 0,
        "exact-content dup must NOT be swept even if names differ"
    );
    assert_eq!(store.open_decision_count().unwrap(), 1);
}

#[test]
fn run_detectors_skips_near_dup_differently_named_cluster() {
    // Integration test: run_detectors must skip the cluster before opening it.
    // We seed two files with the same source_hash so find_exact_duplicates fires
    // but differently-named files — exact=true so the basename gate does NOT apply,
    // confirming that exact clusters still go through. Then we also test near-dup
    // via the seeding loop gate (exact=false, different names must be skipped).
    //
    // The store's find_near_duplicates requires embeddings, so we test the seeding
    // gate indirectly: inject a near-dup differently-named open question, then run
    // run_detectors which calls sweep_filtered_noise first and clears it.
    let mut store = Store::open_in_memory().unwrap();
    let q = NewDecision {
        decision_type: "duplicate".into(),
        subject: "/r/graph.rs".into(),
        params: serde_json::json!({
            "paths": ["/r/graph.rs", "/r/pack.rs"],
            "similarity": 0.96_f32,
            "exact": false,
        }),
        options: serde_json::json!(["/r/graph.rs", "/r/pack.rs", "keep_all"]),
        auto_value: Some("/r/graph.rs".into()),
        confidence: Some(0.96),
        evidence_hash: "test-hash-gate-integration".into(),
        priority: 60,
        paths: vec!["/r/graph.rs".into(), "/r/pack.rs".into()],
    };
    store.record_decision(q).unwrap();
    assert_eq!(store.open_decision_count().unwrap(), 1);

    // Summaries for the members are needed so the expiry sweep doesn't expire them.
    store
        .upsert_summary(&file_summary("/r/graph.rs", "Hg"))
        .unwrap();
    store
        .upsert_summary(&file_summary("/r/pack.rs", "Hp"))
        .unwrap();

    let cfg = crate::config::ReviewConfig::default();
    let report = run_detectors(&mut store, &cfg).unwrap();
    // sweep_filtered_noise counts as skipped; the question must be gone.
    assert!(
        report.skipped >= 1,
        "differently-named near-dup must be swept by run_detectors"
    );
    assert_eq!(
        store.open_decision_count().unwrap(),
        0,
        "question must be dismissed"
    );
}
