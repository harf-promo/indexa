use super::*;

// ── Entry-cleanup integrity contract ──────────────────────────────────────────
// These lock the invariant that deleting an entry leaves NO orphaned rows in any
// entry-keyed child table. There is intentionally no FK ON DELETE CASCADE (the model
// allows entry-less chunks/summaries — deep/summarize without scan), so this manual
// contract is the integrity guarantee. Add any new entry-keyed child table here.

#[test]
fn delete_entry_leaves_no_orphans() {
    let mut store = Store::open_in_memory().unwrap();
    seed_full_entry(&mut store, "/proj/a.rs");
    // Sanity: every child table has a row before deletion.
    assert!(orphan_rows_for(&store, "/proj/a.rs") >= 6);

    let removed = store.delete_entry("/proj/a.rs").unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        orphan_rows_for(&store, "/proj/a.rs"),
        0,
        "delete_entry must clear chunks/fts/edges/summaries/queue/classifications"
    );
    assert_eq!(store.entry_count().unwrap(), 0);
}

#[test]
fn delete_subtree_leaves_no_orphans() {
    let mut store = Store::open_in_memory().unwrap();
    seed_full_entry(&mut store, "/proj/sub/a.rs");
    seed_full_entry(&mut store, "/proj/sub/b.rs");
    // A sibling outside the deleted prefix must survive.
    seed_full_entry(&mut store, "/other/keep.rs");

    store.delete_subtree("/proj/sub").unwrap();

    assert_eq!(orphan_rows_for(&store, "/proj/sub/a.rs"), 0);
    assert_eq!(orphan_rows_for(&store, "/proj/sub/b.rs"), 0);
    // The out-of-scope sibling is untouched.
    assert!(orphan_rows_for(&store, "/other/keep.rs") >= 6);
}

#[test]
fn importance_weights_persist_across_entry_delete() {
    // Documented design: weights are NOT cleared with the entry (unlike classifications).
    let mut store = Store::open_in_memory().unwrap();
    seed_full_entry(&mut store, "/proj/weighted.rs");
    store
        .set_weight("file", "/proj/weighted.rs", 2.0, "user", None)
        .unwrap();

    store.delete_entry("/proj/weighted.rs").unwrap();

    // Entry + all child rows gone, but the weight remains.
    assert_eq!(orphan_rows_for(&store, "/proj/weighted.rs"), 0);
    assert!((store.weight_for("/proj/weighted.rs").unwrap() - 2.0).abs() < 1e-6);
}

#[test]
fn delete_subtree_no_trailing_slash_spares_sibling_prefix() {
    // Regression: delete_subtree("/proj") must remove /proj + /proj/… but NOT /projector
    // (the bug was like_prefix("/proj") = "/proj%", matching the sibling). Callers (indexa rm,
    // DELETE /api/entry) pass unnormalized paths, so the store must normalize internally.
    let mut store = Store::open_in_memory().unwrap();
    seed_full_entry(&mut store, "/proj");
    seed_full_entry(&mut store, "/proj/src/a.rs");
    seed_full_entry(&mut store, "/projector/x.rs"); // sibling sharing the string prefix

    let removed = store.delete_subtree("/proj").unwrap(); // no trailing slash
    assert_eq!(removed, 2, "removes /proj and /proj/src/a.rs only");

    // The subtree is fully gone (no orphans).
    assert_eq!(orphan_rows_for(&store, "/proj"), 0);
    assert_eq!(orphan_rows_for(&store, "/proj/src/a.rs"), 0);
    // The sibling is untouched across every table.
    assert!(
        orphan_rows_for(&store, "/projector/x.rs") >= 6,
        "/projector must survive"
    );
}

#[test]
fn delete_chunks_for_subtree_spares_sibling_prefix() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[
            dummy_chunk("/proj/a.rs", 0, "in scope"),
            dummy_chunk("/projector/b.rs", 0, "sibling — must survive"),
        ])
        .unwrap();
    store.delete_chunks_for_subtree("/proj").unwrap();
    assert_eq!(store.chunk_count().unwrap(), 1, "only /proj/a.rs cleared");
    let hits = store
        .hybrid_search("sibling", None, &HybridMode::Sparse, None, 5, 60.0)
        .unwrap();
    assert!(hits.iter().any(|h| h.entry_path == "/projector/b.rs"));
}

#[test]
fn prune_removes_dangling_rows_but_keeps_entried() {
    let mut store = Store::open_in_memory().unwrap();
    // One entried file; one orphan path (chunks + summary but no entries row).
    store
        .upsert_entries(&[dummy_entry("/keep.rs", EntryKind::File, 10)])
        .unwrap();
    store
        .upsert_chunks(&[
            dummy_chunk_embedded("/keep.rs", 0, "kept content"),
            dummy_chunk_embedded("/orphan.rs", 0, "dangling content"),
        ])
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/orphan.rs", "file", None, 0))
        .unwrap();

    let before = store.count_orphans().unwrap();
    assert_eq!((before.chunks, before.summaries), (1, 1), "one orphan each");

    let removed = store.prune_orphans().unwrap();
    assert_eq!((removed.chunks, removed.summaries), (1, 1));

    // Orphan gone; the entried file's chunk is untouched.
    assert!(store.count_orphans().unwrap().is_empty());
    assert_eq!(store.chunk_count().unwrap(), 1, "/keep.rs chunk preserved");
    let hits = store
        .hybrid_search("content", None, &HybridMode::Sparse, None, 5, 60.0)
        .unwrap();
    assert!(hits.iter().all(|h| h.entry_path == "/keep.rs"));
}

#[test]
fn prune_noops_on_entryless_index() {
    // `deep`/`summarize` without `scan` leaves chunks with zero entries — a legitimate,
    // intentional state. prune must NOT treat the whole index as orphaned and wipe it.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[dummy_chunk_embedded("/a.rs", 0, "deep without scan")])
        .unwrap();
    let removed = store.prune_orphans().unwrap();
    assert_eq!(
        removed.chunks, 0,
        "entry-less index must be preserved, not wiped"
    );
    assert_eq!(store.chunk_count().unwrap(), 1);
}

#[test]
fn summary_provenance_stamp_and_replace() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&dummy_summary("/docs/file.txt", "file", Some("/docs"), 2))
        .unwrap();
    store
        .set_summary_provenance("/docs/file.txt", "ollama", 2, true)
        .unwrap();

    let read = |store: &Store| -> (Option<String>, Option<i64>, Option<i64>) {
        store
            .conn
            .query_row(
                "SELECT provider, passes, fallback FROM summaries WHERE path = '/docs/file.txt'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };
    assert_eq!(
        read(&store),
        (Some("ollama".into()), Some(2), Some(1)),
        "provenance must be stamped onto the summary row"
    );

    // INSERT OR REPLACE on re-summarize clears the old provenance (no stale lineage);
    // the new stamp lands after the new row.
    store
        .upsert_summary(&dummy_summary("/docs/file.txt", "file", Some("/docs"), 2))
        .unwrap();
    assert_eq!(
        read(&store),
        (None, None, None),
        "re-summarize must not carry forward the previous row's provenance"
    );
    store
        .set_summary_provenance("/docs/file.txt", "claude-code", 1, false)
        .unwrap();
    assert_eq!(read(&store), (Some("claude-code".into()), Some(1), Some(0)));
}
