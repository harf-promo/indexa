use super::*;
use crate::store::search::{fts5_quote, like_prefix};

// ── v0.28.2: queue hygiene — honest counts, self-clean, prune report, enqueue guard ──

#[test]
fn queue_stats_excludes_orphan_pending_and_reports_stale() {
    let mut store = Store::open_in_memory().unwrap();
    // Queue an artifact path while the index is still entry-less (the guard is bypassed
    // there) — this is how historical pollution got in.
    store
        .enqueue_summary_items(&[("/proj/.git/HEAD".to_owned(), "file".to_owned(), 5)])
        .unwrap();
    // Now the index has a real entry; the .git row is an orphan (no matching entry).
    store
        .upsert_entries(&[dummy_entry("/proj/real.rs", EntryKind::File, 10)])
        .unwrap();
    store
        .enqueue_summary_items(&[("/proj/real.rs".to_owned(), "file".to_owned(), 1)])
        .unwrap();

    let s = store.queue_stats().unwrap();
    assert_eq!(
        s.pending, 1,
        "only the entry-backed row is real pending work"
    );
    assert_eq!(
        s.stale, 1,
        "the orphan .git row is reported as stale, not pending"
    );
}

#[test]
fn enqueue_guard_skips_non_entry_paths_when_entries_exist() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/proj/real.rs", EntryKind::File, 10)])
        .unwrap();
    // Batch enqueue: only the entry-backed path is queued; the build-artifact path is dropped.
    store
        .enqueue_summary_items(&[
            ("/proj/real.rs".to_owned(), "file".to_owned(), 1),
            ("/proj/target/debug/x.rlib".to_owned(), "file".to_owned(), 3),
        ])
        .unwrap();
    assert!(store.queue_state("/proj/real.rs").unwrap().is_some());
    assert!(
        store
            .queue_state("/proj/target/debug/x.rlib")
            .unwrap()
            .is_none(),
        "a non-entry build-artifact path must not be queued"
    );
    // Single-path resummary (the watch/web path) is guarded too.
    store
        .mark_for_resummary("/proj/.git/COMMIT_EDITMSG", "file", 4)
        .unwrap();
    assert!(store
        .queue_state("/proj/.git/COMMIT_EDITMSG")
        .unwrap()
        .is_none());
}

#[test]
fn prune_reports_and_removes_orphan_queue_rows() {
    let mut store = Store::open_in_memory().unwrap();
    // Orphan queue row (queued entry-less), then a real entry so it becomes an orphan.
    store
        .enqueue_summary_items(&[("/proj/.git/HEAD".to_owned(), "file".to_owned(), 5)])
        .unwrap();
    store
        .upsert_entries(&[dummy_entry("/proj/real.rs", EntryKind::File, 10)])
        .unwrap();

    let counts = store.count_orphans().unwrap();
    assert_eq!(
        counts.queue, 1,
        "count_orphans must include the dead queue row"
    );
    let removed = store.prune_orphans().unwrap();
    assert_eq!(
        removed.queue, 1,
        "prune must report the queue row it deleted"
    );
    assert!(store.queue_state("/proj/.git/HEAD").unwrap().is_none());
}

#[test]
fn unknown_extensions_uses_basename_extension() {
    let mut store = Store::open_in_memory().unwrap();
    // A directory containing a dot plus a multi-dot filename: the old SQL sliced from
    // the FIRST dot in the whole path; the fix must use the true basename extension.
    store
        .upsert_entries(&[
            dummy_entry("/home/user.name/report.tar.gz", EntryKind::File, 1),
            dummy_entry("/home/user.name/notes.gz", EntryKind::File, 1),
            dummy_entry("/home/user.name/README", EntryKind::File, 1),
        ])
        .unwrap();

    let exts = store.unknown_extensions(10).unwrap();
    let gz = exts
        .iter()
        .find(|(e, _)| e == ".gz")
        .expect(".gz must be detected");
    assert_eq!(gz.1, 2, ".gz should count both .tar.gz and .gz files");
    assert!(exts.iter().any(|(e, n)| e == "(no extension)" && *n == 1));
    assert!(
        !exts.iter().any(|(e, _)| e.contains('/')),
        "must not produce a directory-fragment 'extension'"
    );
}

#[test]
fn delete_entry_removes_entry_and_chunks() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/notes.txt", EntryKind::File, 100)])
        .unwrap();
    store
        .upsert_chunks(&[dummy_chunk("/notes.txt", 0, "hello world")])
        .unwrap();
    assert_eq!(store.entry_count().unwrap(), 1);
    assert_eq!(store.chunk_count().unwrap(), 1);

    let deleted = store.delete_entry("/notes.txt").unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(store.entry_count().unwrap(), 0);
    assert_eq!(store.chunk_count().unwrap(), 0);
}

#[test]
fn delete_subtree_removes_all_under_prefix() {
    let mut store = Store::open_in_memory().unwrap();
    let entries = vec![
        dummy_entry("/docs/a.txt", EntryKind::File, 10),
        dummy_entry("/docs/b.txt", EntryKind::File, 10),
        dummy_entry("/other/c.txt", EntryKind::File, 10),
    ];
    store.upsert_entries(&entries).unwrap();
    assert_eq!(store.entry_count().unwrap(), 3);

    let deleted = store.delete_subtree("/docs/").unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(store.entry_count().unwrap(), 1);
}

#[test]
fn delete_subtree_does_not_over_delete_similar_prefix() {
    // "/foo" delete must NOT remove "/foobar/file.txt"
    let mut store = Store::open_in_memory().unwrap();
    let entries = vec![
        dummy_entry("/foo/a.txt", EntryKind::File, 10),
        dummy_entry("/foobar/b.txt", EntryKind::File, 10),
    ];
    store.upsert_entries(&entries).unwrap();

    store.delete_subtree("/foo/").unwrap();

    assert_eq!(
        store.entry_count().unwrap(),
        1,
        "/foobar/b.txt should survive"
    );
}

#[test]
fn hybrid_search_sparse_mode_returns_fts_results() {
    let mut store = Store::open_in_memory().unwrap();
    let chunks = vec![dummy_chunk("/doc.md", 0, "indexa sparse retrieval test")];
    store.upsert_chunks(&chunks).unwrap();

    let hits = store
        .hybrid_search("sparse", None, &HybridMode::Sparse, None, 5, 60.0)
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].text.contains("sparse"));
}

#[test]
fn hybrid_search_dense_mode_returns_vector_results() {
    let mut store = Store::open_in_memory().unwrap();
    let mut c = dummy_chunk("/vec.md", 0, "dense vector search");
    c.embedding = Some(vec![1.0, 0.0, 0.0]);
    store.upsert_chunks(&[c]).unwrap();

    let query_vec = vec![1.0_f32, 0.0, 0.0];
    let hits = store
        .hybrid_search("dense", Some(&query_vec), &HybridMode::Dense, None, 5, 60.0)
        .unwrap();
    assert!(!hits.is_empty());
}

#[test]
fn hybrid_search_scope_filters_by_path_prefix() {
    let mut store = Store::open_in_memory().unwrap();
    let chunks = vec![
        dummy_chunk("/docs/tax/form.pdf", 0, "tax return income"),
        dummy_chunk("/photos/vacation.jpg", 0, "vacation photo hawaii"),
    ];
    store.upsert_chunks(&chunks).unwrap();

    let hits = store
        .hybrid_search(
            "vacation",
            None,
            &HybridMode::Sparse,
            Some("/docs/"),
            10,
            60.0,
        )
        .unwrap();
    assert!(
        hits.is_empty(),
        "scope /docs/ should exclude /photos/ results"
    );
}

#[test]
fn fts5_quote_escapes_double_quotes() {
    let quoted = fts5_quote(r#"he said "hello""#);
    assert!(quoted.starts_with('"'));
    assert!(quoted.ends_with('"'));
    assert!(
        quoted.contains(r#""""#),
        "embedded quotes should be doubled: {quoted}"
    );
}

#[test]
fn like_prefix_escapes_wildcards_in_path() {
    let p = like_prefix("/home/user/50%_done/");
    assert!(p.contains("\\%"), "% should be escaped: {p}");
    assert!(p.contains("\\_"), "_ should be escaped: {p}");
    assert!(
        p.ends_with('%'),
        "pattern should end with trailing wildcard: {p}"
    );
}

#[test]
fn summaries_upsert_and_lookup() {
    let mut store = Store::open_in_memory().unwrap();
    let rec = dummy_summary("/docs/file.txt", "file", Some("/docs"), 2);
    store.upsert_summary(&rec).unwrap();
    assert_eq!(store.summary_count().unwrap(), 1);

    let got = store.summary_by_path("/docs/file.txt").unwrap().unwrap();
    assert_eq!(got.kind, "file");
    assert_eq!(got.summary, "summary of /docs/file.txt");
    // upsert_summary derives and persists an L0 abstract even when the record
    // was constructed with summary_l0 = None.
    assert_eq!(got.summary_l0.as_deref(), Some("summary of /docs/file.txt"));
}

#[test]
fn abstract_from_takes_first_sentence_and_caps_length() {
    // First sentence only.
    assert_eq!(
        abstract_from("This is the gist. More detail follows here."),
        "This is the gist."
    );
    // No sentence terminator → whole (short) string.
    assert_eq!(abstract_from("Just a label"), "Just a label");
    // Long single sentence is truncated with an ellipsis on a char boundary.
    let long = "x".repeat(200);
    let got = abstract_from(&long);
    assert!(got.ends_with('…'));
    assert!(got.chars().count() <= 121);
    // Does not panic on multibyte content.
    let _ = abstract_from("Café déjà vu — 日本語 résumé. second");
}

#[test]
fn summaries_upsert_is_idempotent() {
    let mut store = Store::open_in_memory().unwrap();
    let rec = dummy_summary("/a.txt", "file", Some("/"), 1);
    store.upsert_summary(&rec).unwrap();
    store.upsert_summary(&rec).unwrap();
    assert_eq!(store.summary_count().unwrap(), 1);
}

#[test]
fn children_summaries_returns_direct_children() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&dummy_summary("/docs/a.txt", "file", Some("/docs"), 2))
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/docs/b.txt", "file", Some("/docs"), 2))
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/other/c.txt", "file", Some("/other"), 2))
        .unwrap();

    let children = store.children_summaries("/docs").unwrap();
    assert_eq!(children.len(), 2);
    assert!(children
        .iter()
        .all(|c| c.parent_path.as_deref() == Some("/docs")));
}

#[test]
fn summary_queue_enqueue_and_dequeue() {
    let mut store = Store::open_in_memory().unwrap();
    let items = vec![
        ("/docs/a.txt".to_owned(), "file".to_owned(), 2i64),
        ("/docs/b.txt".to_owned(), "file".to_owned(), 2i64),
    ];
    store.enqueue_summary_items(&items).unwrap();

    let stats = store.queue_stats().unwrap();
    assert_eq!(stats.pending, 2);

    let item = store.next_queue_item().unwrap().unwrap();
    assert_eq!(item.kind, "file");

    let stats2 = store.queue_stats().unwrap();
    assert_eq!(stats2.in_flight, 1);
    assert_eq!(stats2.pending, 1);

    store.mark_queue_state(&item.path, "done", None).unwrap();
    let stats3 = store.queue_stats().unwrap();
    assert_eq!(stats3.done, 1);
}

#[test]
fn requeue_stale_in_flight_resets_then_caps() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .enqueue_summary_items(&[("/a".to_owned(), "file".to_owned(), 1)])
        .unwrap();

    // Claim → attempts=1, in_flight (simulates a crash mid-processing).
    store.next_queue_item().unwrap().unwrap();
    assert_eq!(store.queue_stats().unwrap().in_flight, 1);

    // Below the cap → requeued to pending.
    assert_eq!(store.requeue_stale_in_flight(3).unwrap(), (1, 0));
    assert_eq!(store.queue_stats().unwrap().pending, 1);

    store.next_queue_item().unwrap().unwrap(); // attempts=2
    assert_eq!(store.requeue_stale_in_flight(3).unwrap(), (1, 0));
    store.next_queue_item().unwrap().unwrap(); // attempts=3 — reaches cap

    // At the cap → failed instead of requeued (it keeps crashing).
    assert_eq!(store.requeue_stale_in_flight(3).unwrap(), (0, 1));
    assert_eq!(store.queue_stats().unwrap().failed, 1);
}

#[test]
fn summary_cosine_search_returns_boosted_results() {
    let mut store = Store::open_in_memory().unwrap();
    let mut root = dummy_summary("/", "dir", None, 0);
    root.embedding = Some(vec![1.0, 0.0, 0.0]);
    let mut leaf = dummy_summary("/docs/file.txt", "file", Some("/docs"), 2);
    leaf.embedding = Some(vec![1.0, 0.0, 0.0]);
    store.upsert_summary(&root).unwrap();
    store.upsert_summary(&leaf).unwrap();

    let results = store
        .summary_cosine_search(&[1.0, 0.0, 0.0], 10, 0.15)
        .unwrap();
    assert!(!results.is_empty());
    // Root (depth=0) should score higher than leaf (depth=2) due to depth boost
    assert_eq!(results[0].0, "/");
}

#[test]
fn classification_roundtrip_and_source_guard() {
    let mut store = Store::open_in_memory().unwrap();

    // Auto suggestion.
    store
        .upsert_auto_classifications(&[("/proj".into(), "dir".into(), "code".into(), 0.9)])
        .unwrap();
    let rec = store.classification_for("/proj").unwrap().unwrap();
    assert_eq!(rec.category, "code");
    assert_eq!(rec.source, "auto");
    assert!((rec.confidence - 0.9).abs() < 1e-6);

    // User confirms a correction → 'user', full confidence, timestamped.
    store.confirm_classification("/proj", "work").unwrap();
    let rec = store.classification_for("/proj").unwrap().unwrap();
    assert_eq!(rec.category, "work");
    assert_eq!(rec.source, "user");
    assert!(rec.confirmed_at.is_some());

    // A later auto pass must NOT overwrite the user's decision.
    store
        .upsert_auto_classifications(&[("/proj".into(), "dir".into(), "code".into(), 0.9)])
        .unwrap();
    let rec = store.classification_for("/proj").unwrap().unwrap();
    assert_eq!(rec.category, "work");
    assert_eq!(rec.source, "user");

    // Ignore is a sticky tombstone; auto does not resurrect it.
    store.ignore_classification("/proj").unwrap();
    store
        .upsert_auto_classifications(&[("/proj".into(), "dir".into(), "code".into(), 0.9)])
        .unwrap();
    assert_eq!(
        store.classification_for("/proj").unwrap().unwrap().source,
        "ignored"
    );

    // The tombstone is excluded from the 'auto' suggestion queue.
    let autos = store.list_classifications(Some("auto"), 0).unwrap();
    assert!(autos.iter().all(|c| c.path != "/proj"));
}

#[test]
fn deleting_an_entry_removes_its_classification() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/a/proj", EntryKind::Dir, 0)])
        .unwrap();
    store
        .upsert_auto_classifications(&[("/a/proj".into(), "dir".into(), "code".into(), 0.9)])
        .unwrap();
    assert_eq!(store.classification_count().unwrap(), 1);

    store.delete_entry("/a/proj").unwrap();
    assert_eq!(store.classification_count().unwrap(), 0);
}

#[test]
fn deleting_a_subtree_removes_classifications_under_it() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_auto_classifications(&[
            ("/root".into(), "dir".into(), "code".into(), 0.9),
            ("/root/sub".into(), "dir".into(), "media".into(), 0.8),
            ("/other".into(), "dir".into(), "system".into(), 0.9),
        ])
        .unwrap();
    assert_eq!(store.classification_count().unwrap(), 3);

    store.delete_subtree("/root").unwrap();
    assert!(store.classification_for("/root").unwrap().is_none());
    assert!(store.classification_for("/root/sub").unwrap().is_none());
    assert!(store.classification_for("/other").unwrap().is_some());
}

#[test]
fn root_paths_returns_indexed_root_not_fs_parent() {
    // Regression (v0.44): root_paths() must return the indexed root *directory*
    // (the project the user actually indexed), not its un-indexed filesystem parent.
    // The old form returned `DISTINCT parent_path`, so `project_overview()` with no
    // scope resolved to the FS parent (which has no summary) and walked *away* from the
    // data — the "what is this project?" entry point came back empty.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/proj/indexa", EntryKind::Dir, 0),
            dummy_entry("/proj/indexa/src", EntryKind::Dir, 0),
            dummy_entry("/proj/indexa/src/a.rs", EntryKind::File, 10),
            // A second, disjoint indexed root (mirrors a multi-project index).
            dummy_entry("/other/app", EntryKind::Dir, 0),
            dummy_entry("/other/app/main.rs", EntryKind::File, 5),
        ])
        .unwrap();
    let roots = store.root_paths().unwrap();
    assert_eq!(
        roots,
        vec!["/other/app".to_string(), "/proj/indexa".to_string()],
        "roots are the indexed dirs, not /proj or /other or any nested child dir"
    );
}

#[test]
fn tree_level_empty_path_lists_roots() {
    // Regression (v0.44): browse_tree("") and the web api_tree first-load call
    // tree_level(""). No row carries an empty parent_path, so the old `= ?1` form
    // returned nothing. An empty path now lists the indexed roots; a non-empty path
    // still lists that node's direct children unchanged.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/proj/indexa", EntryKind::Dir, 0),
            dummy_entry("/proj/indexa/src", EntryKind::Dir, 0),
            dummy_entry("/proj/indexa/src/a.rs", EntryKind::File, 10),
        ])
        .unwrap();
    let nodes = store.tree_level("").unwrap();
    assert_eq!(nodes.len(), 1, "exactly one indexed root");
    assert_eq!(nodes[0].path, "/proj/indexa");
    let children = store.tree_level("/proj/indexa").unwrap();
    let names: Vec<&str> = children.iter().map(|n| n.path.as_str()).collect();
    assert!(
        names.contains(&"/proj/indexa/src"),
        "non-empty path still lists direct children"
    );
}

#[test]
fn tree_level_rolls_up_subtree_coverage() {
    // PR-2: each tree node carries a {covered, partial, total} directory-summary rollup
    // for its subtree, so the UI can show a calm static glyph + determinate count instead
    // of a per-row pending strobe.
    //
    // /root
    //   ├─ a        (dir, summary done)      ┐ subtree {a, a/b} → total 2,
    //   │   └─ b    (dir, summary pending)   ┘                    covered 1, partial 1
    //   ├─ empty    (dir, never enqueued)     → total 1, covered 0, partial 0
    //   ├─ full     (dir, summary done)       → total 1, covered 1, partial 0
    //   └─ file.txt (file)                    → total 0 (files carry no rollup)
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/root", EntryKind::Dir, 0),
            dummy_entry("/root/a", EntryKind::Dir, 0),
            dummy_entry("/root/a/b", EntryKind::Dir, 0),
            dummy_entry("/root/empty", EntryKind::Dir, 0),
            dummy_entry("/root/full", EntryKind::Dir, 0),
            dummy_entry("/root/file.txt", EntryKind::File, 10),
        ])
        .unwrap();
    store
        .enqueue_summary_items(&[
            ("/root/a".to_owned(), "dir".to_owned(), 1),
            ("/root/a/b".to_owned(), "dir".to_owned(), 2),
            ("/root/full".to_owned(), "dir".to_owned(), 1),
        ])
        .unwrap();
    store.mark_queue_state("/root/a", "done", None).unwrap();
    store.mark_queue_state("/root/full", "done", None).unwrap();
    // /root/a/b stays pending.

    let nodes = store.tree_level("/root").unwrap();
    let by = |p: &str| {
        nodes
            .iter()
            .find(|n| n.path == p)
            .unwrap_or_else(|| panic!("missing node {p}"))
    };

    let a = by("/root/a");
    assert_eq!(
        (a.covered, a.partial, a.total),
        (1, 1, 2),
        "partial subtree"
    );

    let empty = by("/root/empty");
    assert_eq!(
        (empty.covered, empty.partial, empty.total),
        (0, 0, 1),
        "no context yet"
    );

    let full = by("/root/full");
    assert_eq!(
        (full.covered, full.partial, full.total),
        (1, 0, 1),
        "fully built"
    );

    let file = by("/root/file.txt");
    assert_eq!(
        (file.covered, file.partial, file.total),
        (0, 0, 0),
        "files carry no rollup"
    );
}

/// Safety net for the set-based `tree_level` rewrite (perf/tree-level): the new
/// aggregating implementation MUST return output byte-for-byte identical to the
/// preserved `tree_level_reference` correctness oracle (the original per-row
/// correlated-subquery SQL). Seeds a RICH tree — multiple roots, several nesting
/// levels, files at various depths, chunks under several files (incl. one at a
/// child's own path to exercise the descendant-only chunk filter), and
/// summary_queue dir rows in done / pending / in_flight states — then asserts
/// full `Vec<TreeNode>` equality (via derived `PartialEq`) at the root level and
/// at several non-root parents. If they ever diverge, this fails.
#[test]
fn tree_level_matches_reference_oracle() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            // Root 1: a deep, mixed subtree.
            dummy_entry("/proj/alpha", EntryKind::Dir, 0),
            dummy_entry("/proj/alpha/src", EntryKind::Dir, 0),
            dummy_entry("/proj/alpha/src/core", EntryKind::Dir, 0),
            dummy_entry("/proj/alpha/src/core/deep", EntryKind::Dir, 0),
            dummy_entry("/proj/alpha/src/lib.rs", EntryKind::File, 120),
            dummy_entry("/proj/alpha/src/core/mod.rs", EntryKind::File, 80),
            dummy_entry("/proj/alpha/src/core/deep/util.rs", EntryKind::File, 40),
            dummy_entry("/proj/alpha/docs", EntryKind::Dir, 0),
            dummy_entry("/proj/alpha/docs/intro.md", EntryKind::File, 30),
            dummy_entry("/proj/alpha/README.md", EntryKind::File, 15),
            dummy_entry("/proj/alpha/empty", EntryKind::Dir, 0),
            // Root 2: a second top-level root (exercises the `parent_path = ''` case
            // where roots share no common ancestor).
            dummy_entry("/proj/beta", EntryKind::Dir, 0),
            dummy_entry("/proj/beta/main.rs", EntryKind::File, 200),
            dummy_entry("/proj/beta/sub", EntryKind::Dir, 0),
            dummy_entry("/proj/beta/sub/a.rs", EntryKind::File, 10),
            dummy_entry("/proj/beta/sub/b.rs", EntryKind::File, 11),
        ])
        .unwrap();

    // Chunks under several files at various depths. Includes a chunk whose entry_path
    // is a DIRECTORY child's own path (`/proj/alpha/docs`) — the reference counts
    // chunks via `LIKE e.path || '/%'` (descendants only), so this must NOT be
    // counted toward `docs`'s chunk_count, only toward `/proj/alpha`'s.
    store
        .upsert_chunks(&[
            dummy_chunk("/proj/alpha/src/lib.rs", 0, "fn lib one"),
            dummy_chunk("/proj/alpha/src/lib.rs", 1, "fn lib two"),
            dummy_chunk("/proj/alpha/src/core/mod.rs", 0, "fn mod core"),
            dummy_chunk("/proj/alpha/src/core/deep/util.rs", 0, "fn deep util"),
            dummy_chunk("/proj/alpha/docs/intro.md", 0, "intro text here"),
            dummy_chunk("/proj/alpha/docs", 0, "chunk AT the dir path itself"),
            dummy_chunk("/proj/alpha/README.md", 0, "readme top level"),
            dummy_chunk("/proj/beta/main.rs", 0, "fn main beta"),
            dummy_chunk("/proj/beta/sub/a.rs", 0, "fn a"),
            dummy_chunk("/proj/beta/sub/b.rs", 0, "fn b"),
        ])
        .unwrap();

    // Dir summary_queue rows across all three states.
    store
        .enqueue_summary_items(&[
            ("/proj/alpha".to_owned(), "dir".to_owned(), 1),
            ("/proj/alpha/src".to_owned(), "dir".to_owned(), 2),
            ("/proj/alpha/src/core".to_owned(), "dir".to_owned(), 3),
            ("/proj/alpha/src/core/deep".to_owned(), "dir".to_owned(), 4),
            ("/proj/alpha/docs".to_owned(), "dir".to_owned(), 2),
            ("/proj/alpha/empty".to_owned(), "dir".to_owned(), 2),
            ("/proj/beta".to_owned(), "dir".to_owned(), 1),
            ("/proj/beta/sub".to_owned(), "dir".to_owned(), 2),
            // A FILE queue row — must never count toward any dir rollup (kind='dir' filter).
            ("/proj/alpha/src/lib.rs".to_owned(), "file".to_owned(), 3),
        ])
        .unwrap();
    store
        .mark_queue_state("/proj/alpha/src", "done", None)
        .unwrap();
    store
        .mark_queue_state("/proj/alpha/src/core", "done", None)
        .unwrap();
    store
        .mark_queue_state("/proj/alpha/src/core/deep", "in_flight", None)
        .unwrap();
    store.mark_queue_state("/proj/beta", "done", None).unwrap();
    // /proj/alpha, /proj/alpha/docs, /proj/alpha/empty, /proj/beta/sub stay 'pending'.

    let assert_same = |p: &str| {
        let got = store.tree_level(p).unwrap();
        let want = store.tree_level_reference(p).unwrap();
        assert_eq!(
            got, want,
            "tree_level({p:?}) diverged from tree_level_reference"
        );
    };

    // Root level + several non-root parents at different depths.
    assert_same("");
    assert_same("/proj/alpha");
    assert_same("/proj/alpha/src");
    assert_same("/proj/alpha/src/core");
    assert_same("/proj/alpha/docs");
    assert_same("/proj/alpha/empty"); // no children → empty vec, both sides
    assert_same("/proj/beta");
    assert_same("/proj/beta/sub");
    assert_same("/nonexistent"); // no children → empty vec
}

#[test]
fn chunks_current_for_mtime_uses_fresh_mtime_not_stored() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[dummy_chunk_embedded("/a.txt", 0, "hello world")])
        .unwrap();
    // The chunk's indexed_at is "now" (SQLite unixepoch at insert).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // Embedded chunk, file last modified well in the past → current → deep skips it.
    assert!(store
        .chunks_current_for_mtime("/a.txt", now - 3600)
        .unwrap());
    // File edited after indexing (mtime in the future) → NOT current → deep re-embeds.
    // This is the bug being fixed: chunks_are_current (stored modified_s) would have
    // wrongly reported current when deep runs without a fresh re-scan.
    assert!(!store
        .chunks_current_for_mtime("/a.txt", now + 3600)
        .unwrap());
    // A path with no chunks is never current.
    assert!(!store
        .chunks_current_for_mtime("/missing.txt", now - 3600)
        .unwrap());
}

#[test]
fn chunks_with_missing_embeddings_are_not_current() {
    // The broken-Ollama trap: chunks exist with a fresh `indexed_at` but were stored
    // without a vector (embed failure). They must NOT count as current, so a re-run of
    // `deep` re-processes the file and fills the missing embeddings instead of skipping it.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[dummy_chunk("/b.txt", 0, "no vector")]) // embedding: None
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        !store
            .chunks_current_for_mtime("/b.txt", now - 3600)
            .unwrap(),
        "a file with un-embedded chunks must be re-processed, not skipped"
    );
    // A mix (one embedded, one not) is also not current — the un-embedded one needs a vector.
    store
        .upsert_chunks(&[
            dummy_chunk_embedded("/c.txt", 0, "has vector"),
            dummy_chunk("/c.txt", 1, "no vector"),
        ])
        .unwrap();
    assert!(
        !store
            .chunks_current_for_mtime("/c.txt", now - 3600)
            .unwrap(),
        "any un-embedded chunk makes the file not-current"
    );
}

#[test]
fn mark_for_resummary_inserts_when_absent() {
    let mut store = Store::open_in_memory().unwrap();
    store.mark_for_resummary("/a.txt", "file", 1).unwrap();
    let s = store.queue_stats().unwrap();
    assert_eq!((s.pending, s.done, s.failed), (1, 0, 0));
}

#[test]
fn mark_for_resummary_resets_done_and_failed_rows() {
    let mut store = Store::open_in_memory().unwrap();
    // A `done` row must flip back to pending (INSERT OR IGNORE could not).
    store
        .enqueue_summary_items(&[("/done.txt".into(), "file".into(), 1)])
        .unwrap();
    store.mark_queue_state("/done.txt", "done", None).unwrap();
    // A `failed` row claimed once (attempts→1, error set) must flip back too,
    // clearing attempts + error so it gets fresh retries.
    store
        .enqueue_summary_items(&[("/fail.txt".into(), "file".into(), 2)])
        .unwrap();
    let claimed = store.next_queue_item().unwrap().unwrap(); // deepest first → /fail.txt
    assert_eq!(claimed.path, "/fail.txt");
    store
        .mark_queue_state("/fail.txt", "failed", Some("boom"))
        .unwrap();

    store.mark_for_resummary("/done.txt", "file", 1).unwrap();
    store.mark_for_resummary("/fail.txt", "file", 2).unwrap();

    let s = store.queue_stats().unwrap();
    assert_eq!((s.pending, s.done, s.failed), (2, 0, 0));
    let (attempts, error): (i64, Option<String>) = store
        .conn
        .query_row(
            "SELECT attempts, error FROM summary_queue WHERE path='/fail.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempts, 0, "attempts reset");
    assert_eq!(error, None, "error cleared");
}

#[test]
fn mark_for_resummary_does_not_reset_in_flight() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .enqueue_summary_items(&[("/busy.txt".into(), "file".into(), 1)])
        .unwrap();
    // A worker claims it → in_flight.
    assert_eq!(store.next_queue_item().unwrap().unwrap().path, "/busy.txt");
    // A concurrent edit must NOT reset it (that would let a second worker re-claim
    // the path the first is mid-summary on — the double-claim next_queue_item prevents).
    store.mark_for_resummary("/busy.txt", "file", 1).unwrap();
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM summary_queue WHERE path='/busy.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "in_flight", "in_flight row left untouched");
}

#[test]
fn subtree_has_unfinished_tracks_children() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .enqueue_summary_items(&[
            ("/proj".into(), "dir".into(), 1),
            ("/proj/f.txt".into(), "file".into(), 2),
        ])
        .unwrap();
    assert!(
        store.subtree_has_unfinished("/proj", 1).unwrap(),
        "pending child"
    );
    // Claim the child → in_flight still counts as unfinished.
    assert_eq!(
        store.next_queue_item().unwrap().unwrap().path,
        "/proj/f.txt"
    );
    assert!(
        store.subtree_has_unfinished("/proj", 1).unwrap(),
        "in_flight child"
    );
    // Done → no longer unfinished.
    store.mark_queue_state("/proj/f.txt", "done", None).unwrap();
    assert!(
        !store.subtree_has_unfinished("/proj", 1).unwrap(),
        "child done"
    );
}

#[test]
fn subtree_has_unfinished_failed_child_is_terminal() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .enqueue_summary_items(&[
            ("/a".into(), "dir".into(), 1),
            ("/a/f.txt".into(), "file".into(), 2),
        ])
        .unwrap();
    store
        .mark_queue_state("/a/f.txt", "failed", Some("x"))
        .unwrap();
    assert!(
        !store.subtree_has_unfinished("/a", 1).unwrap(),
        "a failed child must not block the dir forever"
    );
}

#[test]
fn subtree_has_unfinished_guards_prefix_siblings() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .enqueue_summary_items(&[
            ("/proj".into(), "dir".into(), 1),
            ("/projector/x.txt".into(), "file".into(), 2),
        ])
        .unwrap();
    // /projector/x.txt is a prefix-sibling, NOT inside /proj's subtree → /proj not blocked.
    assert!(!store.subtree_has_unfinished("/proj", 1).unwrap());
}
