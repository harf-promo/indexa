//! Typed durable memory storage. The three contracts asserted hardest here are the ones that,
//! if they silently broke, would make the feature actively harmful rather than merely wrong:
//! decay eligibility, never-delete, and not being orphan-pruned.

use super::*;
use crate::memory::{Author, MemoryKind, MemoryStatus, NewMemory, VerifyStatus};

fn mem(kind: MemoryKind, text: &str, author: Author) -> NewMemory {
    NewMemory::new(kind, text, author)
}

/// Backdate a row's `created_at` so decay's age cutoff can be exercised without sleeping.
fn backdate(store: &Store, id: i64, secs_ago: i64) {
    store
        .db_connection()
        .execute(
            "UPDATE memories SET created_at = unixepoch() - ?1 WHERE id = ?2",
            rusqlite::params![secs_ago, id],
        )
        .unwrap();
}

fn confidence_of(store: &Store, id: i64) -> f32 {
    store.memory_by_id(id).unwrap().unwrap().confidence
}

#[test]
fn record_and_read_back_round_trips_every_field() {
    let mut store = Store::open_in_memory().unwrap();
    let mut m = mem(
        MemoryKind::Observed,
        "the web UI has no bundler",
        Author::Operator,
    );
    m.subject = "crates/web/src/lib.rs".into();
    m.confidence = 0.95;
    m.source_path = Some("crates/web/src/lib.rs".into());
    m.source_sha256 = Some("abc123".into());
    m.verify_cmd = Some("cargo test -p indexa-web".into());
    m.tags = vec!["build".into(), "invariant".into()];
    m.paths = vec![
        "crates/web/src/lib.rs".into(),
        "crates/web/assets/ui".into(),
    ];

    let (id, clamped) = store.record_memory(&m).unwrap();
    assert!(!clamped, "an operator may claim 0.95");

    let got = store.memory_by_id(id).unwrap().expect("row exists");
    assert_eq!(got.kind, MemoryKind::Observed);
    assert_eq!(got.text, "the web UI has no bundler");
    assert_eq!(got.subject, "crates/web/src/lib.rs");
    assert!((got.confidence - 0.95).abs() < 1e-6);
    assert_eq!(got.author, Author::Operator);
    assert_eq!(got.verify_cmd.as_deref(), Some("cargo test -p indexa-web"));
    assert_eq!(got.tags, vec!["build", "invariant"]);
    assert_eq!(got.status, MemoryStatus::Active);
    assert_eq!(got.verify_status, VerifyStatus::Unverified);
    assert_eq!(got.valid_to, None, "a new claim is open-ended");
    assert_eq!(got.paths.len(), 2, "memory_paths hydrated on read");
}

#[test]
fn recording_the_same_claim_twice_returns_the_existing_row() {
    // What makes it safe for an agent to `remember` the same thing every session without the
    // store filling with near-identical rows.
    let mut store = Store::open_in_memory().unwrap();
    let m = mem(MemoryKind::Stated, "we ship on Fridays", Author::Operator);
    let (first, _) = store.record_memory(&m).unwrap();
    let (second, _) = store.record_memory(&m).unwrap();
    assert_eq!(first, second);
    assert_eq!(store.memory_counts().unwrap().active, 1);
}

#[test]
fn a_retired_claim_frees_its_text_for_relearning() {
    // Dedup is scoped to ACTIVE rows on purpose: retiring a claim and later learning it again
    // must produce a new row, not collide with the tombstone.
    let mut store = Store::open_in_memory().unwrap();
    let m = mem(MemoryKind::Inferred, "the flake is a race", Author::Agent);
    let (first, _) = store.record_memory(&m).unwrap();
    assert!(store.retire_memory(first).unwrap());
    let (second, _) = store.record_memory(&m).unwrap();
    assert_ne!(first, second, "a retired row must not block re-learning");
}

#[test]
fn an_agents_confidence_is_clamped_and_the_clamp_is_reported() {
    let mut store = Store::open_in_memory().unwrap();
    let mut m = mem(MemoryKind::Inferred, "probably a race", Author::Agent);
    m.confidence = 0.99;
    let (id, clamped) = store.record_memory(&m).unwrap();
    assert!(clamped, "the caller must be told its number was reduced");
    assert!((confidence_of(&store, id) - 0.75).abs() < 1e-6);
}

// ── The decay contract ────────────────────────────────────────────────────────

#[test]
fn decay_ages_only_unverified_inference_and_hypothesis() {
    let mut store = Store::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for kind in MemoryKind::ALL {
        let mut m = mem(kind, &format!("claim about {kind}"), Author::Operator);
        m.confidence = 0.8;
        let (id, _) = store.record_memory(&m).unwrap();
        backdate(&store, id, 100 * 86_400);
        ids.push((kind, id));
    }

    let changed = store.age_unverified_memories(90 * 86_400, 0.5).unwrap();
    assert_eq!(changed, 2, "exactly inferred + hypothesis");

    for (kind, id) in ids {
        let got = store.memory_by_id(id).unwrap().unwrap();
        if kind.decayable() {
            assert_eq!(got.verify_status, VerifyStatus::Aged, "{kind} should age");
            assert!(
                (got.confidence - 0.4).abs() < 1e-6,
                "{kind} confidence should halve, got {}",
                got.confidence
            );
        } else {
            assert_eq!(
                got.verify_status,
                VerifyStatus::Unverified,
                "{kind} must never be aged — it does not become less true because time passed"
            );
            assert!((got.confidence - 0.8).abs() < 1e-6, "{kind} untouched");
        }
    }
}

#[test]
fn decay_never_touches_a_verified_claim() {
    let mut store = Store::open_in_memory().unwrap();
    let mut m = mem(MemoryKind::Inferred, "checked and held", Author::Operator);
    m.confidence = 0.8;
    let (id, _) = store.record_memory(&m).unwrap();
    backdate(&store, id, 365 * 86_400);
    store
        .set_memory_verify_status(id, VerifyStatus::Verified, Some(1))
        .unwrap();

    assert_eq!(store.age_unverified_memories(1, 0.1).unwrap(), 0);
    let got = store.memory_by_id(id).unwrap().unwrap();
    assert_eq!(got.verify_status, VerifyStatus::Verified);
    assert!((got.confidence - 0.8).abs() < 1e-6);
}

#[test]
fn decay_respects_the_age_cutoff() {
    let mut store = Store::open_in_memory().unwrap();
    let (young, _) = store
        .record_memory(&mem(MemoryKind::Hypothesis, "recent guess", Author::Agent))
        .unwrap();
    let (old, _) = store
        .record_memory(&mem(MemoryKind::Hypothesis, "stale guess", Author::Agent))
        .unwrap();
    backdate(&store, old, 100 * 86_400);

    assert_eq!(store.age_unverified_memories(90 * 86_400, 0.5).unwrap(), 1);
    assert_eq!(
        store.memory_by_id(young).unwrap().unwrap().verify_status,
        VerifyStatus::Unverified
    );
    assert_eq!(
        store.memory_by_id(old).unwrap().unwrap().verify_status,
        VerifyStatus::Aged
    );
}

#[test]
fn decay_never_deletes_and_never_reaches_zero() {
    // Aging is a confidence signal, not a delete, and an aged claim must not read as
    // "definitely false" when what is meant is "nobody ever checked".
    let mut store = Store::open_in_memory().unwrap();
    let mut m = mem(MemoryKind::Hypothesis, "a very old guess", Author::Agent);
    m.confidence = 0.7;
    let (id, _) = store.record_memory(&m).unwrap();
    backdate(&store, id, 10_000 * 86_400);

    for _ in 0..50 {
        store.age_unverified_memories(1, 0.5).unwrap();
    }
    let got = store.memory_by_id(id).unwrap().expect("still present");
    assert!(got.confidence > 0.0, "floored, never zeroed");
    assert_eq!(store.memory_counts().unwrap().active, 1);
}

#[test]
fn decay_is_idempotent_in_kind_but_monotone_in_confidence() {
    let mut store = Store::open_in_memory().unwrap();
    let mut m = mem(MemoryKind::Inferred, "an inference", Author::Operator);
    m.confidence = 1.0;
    let (id, _) = store.record_memory(&m).unwrap();
    backdate(&store, id, 100 * 86_400);

    store.age_unverified_memories(1, 0.5).unwrap();
    let first = confidence_of(&store, id);
    // Now `aged`, no longer `unverified`, so a second pass is a no-op — an operator running
    // decay weekly must not compound it into oblivion.
    assert_eq!(store.age_unverified_memories(1, 0.5).unwrap(), 0);
    assert!((confidence_of(&store, id) - first).abs() < 1e-6);
}

// ── Supersession, validity, retrieval filters ────────────────────────────────

#[test]
fn supersession_chains_both_directions_and_retires_the_old_row() {
    let mut store = Store::open_in_memory().unwrap();
    let (old, _) = store
        .record_memory(&mem(MemoryKind::Stated, "we use gemma2", Author::Operator))
        .unwrap();
    let (new, _) = store
        .supersede_memory(
            old,
            &mem(MemoryKind::Stated, "we use gemma3", Author::Operator),
        )
        .unwrap();

    let prev = store.memory_by_id(old).unwrap().unwrap();
    let next = store.memory_by_id(new).unwrap().unwrap();
    assert_eq!(prev.superseded_by, Some(new));
    assert_eq!(prev.status, MemoryStatus::Retired, "kept, not deleted");
    assert_eq!(next.parent_id, Some(old));
    assert_eq!(next.status, MemoryStatus::Active);
}

#[test]
fn superseding_with_identical_text_does_not_chain_a_row_to_itself() {
    let mut store = Store::open_in_memory().unwrap();
    let m = mem(MemoryKind::Stated, "unchanged", Author::Operator);
    let (old, _) = store.record_memory(&m).unwrap();
    let (new, _) = store.supersede_memory(old, &m).unwrap();
    assert_eq!(old, new);
    let got = store.memory_by_id(old).unwrap().unwrap();
    assert_eq!(got.superseded_by, None);
    assert_eq!(got.status, MemoryStatus::Active);
}

#[test]
fn failed_retired_and_expired_claims_are_excluded_from_retrieval() {
    let mut store = Store::open_in_memory().unwrap();
    let (ok, _) = store
        .record_memory(&mem(MemoryKind::Observed, "still true", Author::Operator))
        .unwrap();
    let (failed, _) = store
        .record_memory(&mem(MemoryKind::Observed, "was wrong", Author::Operator))
        .unwrap();
    let (retired, _) = store
        .record_memory(&mem(MemoryKind::Observed, "withdrawn", Author::Operator))
        .unwrap();
    let (expired, _) = store
        .record_memory(&mem(
            MemoryKind::Observed,
            "no longer true",
            Author::Operator,
        ))
        .unwrap();

    store
        .set_memory_verify_status(failed, VerifyStatus::Failed, None)
        .unwrap();
    store.retire_memory(retired).unwrap();
    store.set_memory_valid_to(expired, 1).unwrap();

    let live: Vec<i64> = store
        .active_memories(None, 0.0, 50)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(live, vec![ok], "only the still-true claim is retrievable");
    assert!(
        store.memory_by_id(failed).unwrap().is_some(),
        "excluded from retrieval, still readable by id — the record is kept"
    );
}

#[test]
fn active_memories_orders_by_trust_then_confidence() {
    let mut store = Store::open_in_memory().unwrap();
    let mut guess = mem(
        MemoryKind::Hypothesis,
        "a confident guess",
        Author::Operator,
    );
    guess.confidence = 1.0;
    let mut seen = mem(
        MemoryKind::Observed,
        "a hedged observation",
        Author::Operator,
    );
    seen.confidence = 0.6;
    store.record_memory(&guess).unwrap();
    store.record_memory(&seen).unwrap();

    let got = store.active_memories(None, 0.0, 10).unwrap();
    assert_eq!(
        got[0].kind,
        MemoryKind::Observed,
        "a witnessed claim outranks a guess even at lower confidence"
    );
}

#[test]
fn active_memories_filters_by_kind_and_confidence_floor() {
    let mut store = Store::open_in_memory().unwrap();
    let mut low = mem(MemoryKind::Inferred, "weak inference", Author::Operator);
    low.confidence = 0.2;
    let mut high = mem(MemoryKind::Inferred, "strong inference", Author::Operator);
    high.confidence = 0.9;
    let mut stated = mem(MemoryKind::Stated, "a statement", Author::Operator);
    stated.confidence = 0.9;
    for m in [&low, &high, &stated] {
        store.record_memory(m).unwrap();
    }

    assert_eq!(store.active_memories(None, 0.5, 10).unwrap().len(), 2);
    let only_stated = store
        .active_memories(Some(&[MemoryKind::Stated]), 0.0, 10)
        .unwrap();
    assert_eq!(only_stated.len(), 1);
    assert_eq!(only_stated[0].kind, MemoryKind::Stated);
}

#[test]
fn memories_are_found_by_subject_or_by_an_associated_path() {
    let mut store = Store::open_in_memory().unwrap();
    let mut by_subject = mem(MemoryKind::Observed, "about the store", Author::Operator);
    by_subject.subject = "/r/store.rs".into();
    let mut by_path = mem(
        MemoryKind::Observed,
        "also about the store",
        Author::Operator,
    );
    by_path.paths = vec!["/r/store.rs".into()];
    let unrelated = mem(
        MemoryKind::Observed,
        "about something else",
        Author::Operator,
    );
    for m in [&by_subject, &by_path, &unrelated] {
        store.record_memory(m).unwrap();
    }

    let hits = store
        .memories_for_paths(&["/r/store.rs".to_owned()], 10)
        .unwrap();
    assert_eq!(hits.len(), 2, "subject match and memory_paths match");
    assert!(hits.iter().all(|m| m.text.contains("about the store")));
    assert!(store.memories_for_paths(&[], 10).unwrap().is_empty());
}

#[test]
fn full_text_search_finds_a_claim_by_its_words() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .record_memory(&mem(
            MemoryKind::Observed,
            "the retrieval pipeline applies an archive penalty",
            Author::Operator,
        ))
        .unwrap();
    store
        .record_memory(&mem(
            MemoryKind::Observed,
            "the desktop app has its own lockfile",
            Author::Operator,
        ))
        .unwrap();

    let hits = store.search_memories("archive penalty", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("archive penalty"));
    assert!(store
        .search_memories("nonexistentterm", 10)
        .unwrap()
        .is_empty());
}

#[test]
fn search_excludes_a_failed_claim() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, _) = store
        .record_memory(&mem(
            MemoryKind::Observed,
            "a claim about tokenization",
            Author::Operator,
        ))
        .unwrap();
    assert_eq!(store.search_memories("tokenization", 10).unwrap().len(), 1);
    store
        .set_memory_verify_status(id, VerifyStatus::Failed, None)
        .unwrap();
    assert!(
        store
            .search_memories("tokenization", 10)
            .unwrap()
            .is_empty(),
        "a claim known to be wrong must not surface in search"
    );
}

// ── The two structural contracts ─────────────────────────────────────────────

#[test]
fn deleting_an_entry_does_not_delete_its_memories() {
    // The whole point. A note explaining why a file was removed is at its most valuable
    // exactly when the file is gone; `prune.rs`'s orphan_rows_for deliberately omits this
    // table, and this test is what stops someone helpfully adding it.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/r/gone.rs", EntryKind::File, 10)])
        .unwrap();
    let mut m = mem(
        MemoryKind::Observed,
        "removed gone.rs because it duplicated helper.rs",
        Author::Operator,
    );
    m.subject = "/r/gone.rs".into();
    m.paths = vec!["/r/gone.rs".into()];
    let (id, _) = store.record_memory(&m).unwrap();

    store.delete_entry("/r/gone.rs").unwrap();

    assert_eq!(store.entry_count().unwrap(), 0, "the entry is gone");
    let got = store
        .memory_by_id(id)
        .unwrap()
        .expect("the memory about it must survive");
    assert_eq!(got.status, MemoryStatus::Active);
    assert_eq!(
        store
            .memories_for_paths(&["/r/gone.rs".to_owned()], 10)
            .unwrap()
            .len(),
        1,
        "and stays findable by the path it is about"
    );
}

#[test]
fn a_v10_database_migrates_to_the_memories_schema_on_reopen() {
    // The migration path every existing user takes. `Store::open` fast-paths out when
    // `user_version` already equals SCHEMA_VERSION, so a DB stamped at 10 must run the DDL and
    // re-stamp — this is the guard that catches a version bump that was forgotten.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let store = Store::open(&db).unwrap();
        store
            .db_connection()
            .execute_batch(
                "DROP TABLE IF EXISTS memories;
                 DROP TABLE IF EXISTS memory_paths;
                 DROP TABLE IF EXISTS memories_fts;
                 PRAGMA user_version = 10;",
            )
            .unwrap();
    }

    let mut store = Store::open(&db).unwrap();
    let (id, _) = store
        .record_memory(&mem(
            MemoryKind::Observed,
            "after migration",
            Author::Operator,
        ))
        .unwrap();
    assert!(store.memory_by_id(id).unwrap().is_some());

    let stamped: i64 = store
        .db_connection()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stamped, crate::store::schema::SCHEMA_VERSION);
}

#[test]
fn counts_split_by_lifecycle_and_verification() {
    let mut store = Store::open_in_memory().unwrap();
    let (a, _) = store
        .record_memory(&mem(MemoryKind::Observed, "one", Author::Operator))
        .unwrap();
    let (b, _) = store
        .record_memory(&mem(MemoryKind::Observed, "two", Author::Operator))
        .unwrap();
    store
        .record_memory(&mem(MemoryKind::Observed, "three", Author::Operator))
        .unwrap();
    store
        .set_memory_verify_status(a, VerifyStatus::Verified, None)
        .unwrap();
    store.retire_memory(b).unwrap();

    let c = store.memory_counts().unwrap();
    assert_eq!((c.active, c.retired), (2, 1));
    assert_eq!((c.verified, c.unverified), (1, 2));
}
