use super::search::{fts5_quote, like_prefix};
use super::*;
use crate::config::HybridMode;
use crate::walker::{Entry, EntryKind};
use std::path::PathBuf;

fn dummy_entry(path: &str, kind: EntryKind, size: u64) -> Entry {
    Entry {
        path: PathBuf::from(path),
        kind,
        size,
        modified: None,
        hint: None,
    }
}

fn dummy_chunk(path: &str, seq: usize, text: &str) -> ChunkRecord {
    ChunkRecord {
        entry_path: path.to_owned(),
        seq,
        heading: String::new(),
        text: text.to_owned(),
        language: None,
        embedding: None,
        embed_model: None,
    }
}

#[test]
fn open_in_memory_and_upsert() {
    let mut store = Store::open_in_memory().unwrap();
    let entries = vec![
        dummy_entry("/home/user/file.txt", EntryKind::File, 1024),
        dummy_entry("/home/user/docs", EntryKind::Dir, 0),
    ];
    store.upsert_entries(&entries).unwrap();
    assert_eq!(store.entry_count().unwrap(), 2);
}

#[test]
fn upsert_is_idempotent() {
    let mut store = Store::open_in_memory().unwrap();
    let e = vec![dummy_entry("/tmp/a.txt", EntryKind::File, 10)];
    store.upsert_entries(&e).unwrap();
    store.upsert_entries(&e).unwrap();
    assert_eq!(store.entry_count().unwrap(), 1);
}

#[test]
fn region_summary_groups_by_category() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/a.txt", EntryKind::File, 100)])
        .unwrap();
    let summary = store.region_summary().unwrap();
    assert!(!summary.is_empty());
}

#[test]
fn chunks_indexed_and_fts_searchable() {
    let mut store = Store::open_in_memory().unwrap();
    let chunks = vec![
        dummy_chunk("/doc.md", 0, "the quick brown fox jumps over the lazy dog"),
        dummy_chunk(
            "/doc.md",
            1,
            "machine learning fundamentals and neural networks",
        ),
    ];
    store.upsert_chunks(&chunks).unwrap();
    assert_eq!(store.chunk_count().unwrap(), 2);

    let hits = store
        .hybrid_search("machine learning", None, &HybridMode::Rrf, None, 10, 60.0)
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].text.contains("machine learning"));
}

#[test]
fn hybrid_search_with_embedding() {
    let mut store = Store::open_in_memory().unwrap();
    // Simple 3-dim embeddings for test
    let mut c1 = dummy_chunk("/a.md", 0, "cats and kittens");
    c1.embedding = Some(vec![1.0, 0.0, 0.0]);
    let mut c2 = dummy_chunk("/b.md", 0, "dogs and puppies");
    c2.embedding = Some(vec![0.0, 1.0, 0.0]);
    store.upsert_chunks(&[c1, c2]).unwrap();

    let query_vec = vec![1.0_f32, 0.0, 0.0];
    let hits = store
        .hybrid_search("cats", Some(&query_vec), &HybridMode::Rrf, None, 10, 60.0)
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].entry_path.contains("/a.md"));
}

#[test]
fn chunk_upsert_is_idempotent() {
    let mut store = Store::open_in_memory().unwrap();
    let c = dummy_chunk("/x.txt", 0, "hello world");
    store.upsert_chunks(std::slice::from_ref(&c)).unwrap();
    store.upsert_chunks(&[c]).unwrap();
    assert_eq!(store.chunk_count().unwrap(), 1);
}

#[test]
fn chunk_ids_are_not_reused_after_delete() {
    // AUTOINCREMENT guarantee — load-bearing for the ANN index: a deleted chunk's id must
    // never be reassigned to a different chunk (else a stale ANN node mis-attributes content).
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[
            dummy_chunk("/a.txt", 0, "one"),
            dummy_chunk("/b.txt", 0, "two"),
        ])
        .unwrap();
    let max_before = store.max_chunk_id().unwrap();
    assert_eq!(max_before, 2);
    store.delete_chunks_for("/b.txt").unwrap(); // frees the max id
    store
        .upsert_chunks(&[dummy_chunk("/c.txt", 0, "three")])
        .unwrap();
    assert!(
        store.max_chunk_id().unwrap() > max_before,
        "AUTOINCREMENT must not reuse the freed id (got {})",
        store.max_chunk_id().unwrap()
    );
}

#[test]
fn migrates_legacy_chunks_to_autoincrement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    {
        // Hand-build a pre-AUTOINCREMENT chunks table (bare rowid) with one row at id 5.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE chunks (
                id INTEGER PRIMARY KEY, entry_path TEXT NOT NULL, seq INTEGER NOT NULL,
                heading TEXT NOT NULL DEFAULT '', text TEXT NOT NULL, language TEXT,
                embedding BLOB, embed_model TEXT,
                indexed_at INTEGER NOT NULL DEFAULT (unixepoch()), UNIQUE(entry_path, seq));
             INSERT INTO chunks (id, entry_path, seq, text) VALUES (5, '/x.txt', 0, 'legacy');",
        )
        .unwrap();
    }
    // Opening runs init_schema → detects the missing AUTOINCREMENT → migrates.
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.max_chunk_id().unwrap(),
        5,
        "migration must preserve ids"
    );
    assert_eq!(store.chunk_count().unwrap(), 1);
    // Post-migration the table has AUTOINCREMENT: deleting id 5 then inserting must not reuse 5.
    store.delete_chunks_for("/x.txt").unwrap();
    store
        .upsert_chunks(&[dummy_chunk("/y.txt", 0, "new")])
        .unwrap();
    assert!(
        store.max_chunk_id().unwrap() > 5,
        "post-migration ids must not be reused (got {})",
        store.max_chunk_id().unwrap()
    );
}

fn fts_row_count(store: &Store) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn reindex_replaces_chunks_and_keeps_fts_in_sync() {
    let mut store = Store::open_in_memory().unwrap();
    // Two files so the chunks table has several rowids — the old `INSERT OR REPLACE`
    // bug only orphaned FTS rows when the replaced chunk was not the table's max rowid.
    store
        .upsert_chunks(&[
            dummy_chunk("/a.txt", 0, "alpha keep"),
            dummy_chunk("/a.txt", 1, "beta middle"),
            dummy_chunk("/a.txt", 2, "gamma tail"),
            dummy_chunk("/b.txt", 0, "delta other"),
        ])
        .unwrap();
    assert_eq!(store.chunk_count().unwrap(), 4);
    assert_eq!(fts_row_count(&store), 4);

    // Re-index /a.txt shrunk from 3 chunks down to 1.
    store
        .upsert_chunks(&[dummy_chunk("/a.txt", 0, "alpha keep updated")])
        .unwrap();

    // /a.txt now has exactly 1 chunk; /b.txt untouched → 2 total. FTS must match the
    // chunk count exactly: no orphaned rows, no stale tail chunk left behind.
    assert_eq!(store.chunk_count().unwrap(), 2);
    assert_eq!(
        fts_row_count(&store),
        2,
        "FTS rows must equal chunk rows after a shrinking re-index (no orphans)"
    );

    // The removed tail content must no longer be searchable.
    let gamma = store
        .hybrid_search("gamma", None, &HybridMode::Sparse, None, 10, 60.0)
        .unwrap();
    assert!(gamma.is_empty(), "stale tail chunk 'gamma' should be gone");

    // The surviving file is still searchable.
    let delta = store
        .hybrid_search("delta", None, &HybridMode::Sparse, None, 10, 60.0)
        .unwrap();
    assert_eq!(delta.len(), 1);
    assert!(delta[0].entry_path.contains("/b.txt"));
}

#[test]
fn delete_subtree_clears_summaries_and_queue() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&dummy_summary("/docs/a.txt", "file", Some("/docs"), 1))
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/docs/b.txt", "file", Some("/docs"), 1))
        .unwrap();
    store
        .upsert_summary(&dummy_summary("/other/c.txt", "file", Some("/other"), 1))
        .unwrap();
    store
        .enqueue_summary_items(&[
            ("/docs/a.txt".to_owned(), "file".to_owned(), 1),
            ("/other/c.txt".to_owned(), "file".to_owned(), 1),
        ])
        .unwrap();

    store.delete_subtree("/docs/").unwrap();

    assert!(store.summary_by_path("/docs/a.txt").unwrap().is_none());
    assert!(store.summary_by_path("/docs/b.txt").unwrap().is_none());
    assert!(
        store.summary_by_path("/other/c.txt").unwrap().is_some(),
        "summary outside the deleted subtree must survive"
    );
    let stats = store.queue_stats().unwrap();
    assert_eq!(
        stats.pending, 1,
        "the /docs queue row must be cleared; /other remains"
    );
}

#[test]
fn delete_entry_clears_summary_and_queue() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_summary(&dummy_summary("/notes.txt", "file", None, 0))
        .unwrap();
    store
        .enqueue_summary_items(&[("/notes.txt".to_owned(), "file".to_owned(), 0)])
        .unwrap();

    store.delete_entry("/notes.txt").unwrap();

    assert!(store.summary_by_path("/notes.txt").unwrap().is_none());
    assert_eq!(store.queue_stats().unwrap().pending, 0);
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

fn dummy_summary(path: &str, kind: &str, parent: Option<&str>, depth: i64) -> SummaryRecord {
    SummaryRecord {
        path: path.to_owned(),
        kind: kind.to_owned(),
        parent_path: parent.map(|s| s.to_owned()),
        depth,
        summary: format!("summary of {path}"),
        summary_l0: None,
        embedding: None,
        child_count: 0,
        byte_size: 100,
        model: "gemma2:2b".to_owned(),
        source_hash: String::new(),
        generated_at: 0,
    }
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

#[test]
fn chunks_current_for_mtime_uses_fresh_mtime_not_stored() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[dummy_chunk("/a.txt", 0, "hello world")])
        .unwrap();
    // The chunk's indexed_at is "now" (SQLite unixepoch at insert).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // File last modified well in the past → chunks are current → deep skips it.
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

fn edge(from: &str, kind: &str, to: &str) -> EdgeRecord {
    EdgeRecord {
        from_path: from.into(),
        kind: kind.into(),
        to_ref: to.into(),
    }
}

#[test]
fn edges_upsert_query_and_reverse_lookup() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/a.rs", "imports", "std::fs"),
            edge("/a.rs", "defines", "run"),
            edge("/b.rs", "imports", "std::fs"),
        ])
        .unwrap();

    let from_a = store.edges_from("/a.rs").unwrap();
    assert_eq!(from_a.len(), 2);
    assert!(from_a
        .iter()
        .any(|e| e.kind == "imports" && e.to_ref == "std::fs"));
    assert!(from_a
        .iter()
        .any(|e| e.kind == "defines" && e.to_ref == "run"));

    // Reverse: both files import std::fs (sorted), only /a.rs defines `run`.
    assert_eq!(
        store.edges_to("imports", "std::fs").unwrap(),
        vec!["/a.rs".to_string(), "/b.rs".to_string()]
    );
    assert_eq!(
        store.edges_to("defines", "run").unwrap(),
        vec!["/a.rs".to_string()]
    );
}

#[test]
fn edges_reupsert_replaces_only_that_file() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/a.rs", "imports", "std::fs"),
            edge("/b.rs", "imports", "std::fs"),
        ])
        .unwrap();

    // Re-deep of /a.rs with a different edge set drops its stale rows, leaves /b.rs.
    store
        .upsert_edges(&[edge("/a.rs", "imports", "std::io")])
        .unwrap();
    let from_a = store.edges_from("/a.rs").unwrap();
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_a[0].to_ref, "std::io");
    assert_eq!(
        store.edges_to("imports", "std::fs").unwrap(),
        vec!["/b.rs".to_string()]
    );
}

#[test]
fn edges_dedup_within_batch_and_cleanup_on_delete() {
    let mut store = Store::open_in_memory().unwrap();
    // Duplicate edge in one batch collapses against the composite PK.
    store
        .upsert_edges(&[edge("/c.rs", "imports", "x"), edge("/c.rs", "imports", "x")])
        .unwrap();
    assert_eq!(store.edges_from("/c.rs").unwrap().len(), 1);

    // Deleting a file's chunks also clears its edges (no orphans).
    store.delete_chunks_for("/c.rs").unwrap();
    assert!(store.edges_from("/c.rs").unwrap().is_empty());
}

#[test]
fn delete_entry_also_removes_edges() {
    // The watcher's file-removal path is delete_entry; it must clear edges too, or
    // who_imports/dependencies keep listing a deleted file.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/gone.rs", EntryKind::File, 1)])
        .unwrap();
    store
        .upsert_edges(&[
            edge("/gone.rs", "imports", "std::fs"),
            edge("/gone.rs", "defines", "run"),
        ])
        .unwrap();
    assert_eq!(store.edges_from("/gone.rs").unwrap().len(), 2);

    store.delete_entry("/gone.rs").unwrap();
    assert!(store.edges_from("/gone.rs").unwrap().is_empty());
    assert!(store.edges_to("imports", "std::fs").unwrap().is_empty());
}
