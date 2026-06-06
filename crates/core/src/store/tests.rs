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

/// Like [`dummy_chunk`] but carries an embedding — for tests that exercise the
/// "all chunks embedded" branch of `chunks_current_for_mtime`.
fn dummy_chunk_embedded(path: &str, seq: usize, text: &str) -> ChunkRecord {
    ChunkRecord {
        embedding: Some(vec![0.1, 0.2, 0.3]),
        embed_model: Some("test".to_owned()),
        ..dummy_chunk(path, seq, text)
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

#[test]
fn legacy_chunks_migration_is_concurrency_safe() {
    // `worker` and `serve` are separate processes on one DB; two could open a legacy DB at
    // once. The migration must be atomic + single-flight so neither errors or corrupts.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_concurrent.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        // WAL like a real Indexa DB (so the concurrent opens don't contend on a journal-mode
        // conversion — the migration's write lock is the only contention under test).
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE chunks (
                id INTEGER PRIMARY KEY, entry_path TEXT NOT NULL, seq INTEGER NOT NULL,
                heading TEXT NOT NULL DEFAULT '', text TEXT NOT NULL, language TEXT,
                embedding BLOB, embed_model TEXT,
                indexed_at INTEGER NOT NULL DEFAULT (unixepoch()), UNIQUE(entry_path, seq));",
        )
        .unwrap();
        for i in 1..=20 {
            conn.execute(
                "INSERT INTO chunks (id, entry_path, seq, text) VALUES (?1, ?2, 0, 'x')",
                rusqlite::params![i, format!("/f{i}.txt")],
            )
            .unwrap();
        }
    }
    // Open from several threads simultaneously — exactly one migrates, none error.
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let p = path.clone();
            std::thread::spawn(move || Store::open(&p).map(|_| ()))
        })
        .collect();
    for h in handles {
        h.join()
            .unwrap()
            .expect("concurrent open must not error during the migration");
    }
    // Data intact, no orphan migrate table, AUTOINCREMENT in effect (no id reuse).
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.chunk_count().unwrap(), 20);
    let leftover: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='chunks_migrate'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0, "no orphan chunks_migrate table left behind");
    store.delete_chunks_for("/f20.txt").unwrap();
    store
        .upsert_chunks(&[dummy_chunk("/new.txt", 0, "n")])
        .unwrap();
    assert!(
        store.max_chunk_id().unwrap() > 20,
        "ids must not be reused post-migration"
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

// ── Context Packs ─────────────────────────────────────────────────────────────

#[test]
fn pack_create_and_lookup_by_name() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .create_pack("Auth", Some("authentication files"))
        .unwrap();
    assert!(!id.is_empty(), "generated id must be non-empty");

    let rec = store.pack_by_name("Auth").unwrap().unwrap();
    assert_eq!(rec.name, "Auth");
    assert_eq!(rec.description.as_deref(), Some("authentication files"));
    assert_eq!(rec.id, id);
    assert_eq!(rec.path_count, 0);
}

#[test]
fn pack_lookup_is_case_insensitive() {
    let mut store = Store::open_in_memory().unwrap();
    store.create_pack("Auth", None).unwrap();

    assert!(store.pack_by_name("auth").unwrap().is_some());
    assert!(store.pack_by_name("AUTH").unwrap().is_some());
    assert!(store.pack_by_name("aUtH").unwrap().is_some());
}

#[test]
fn pack_lookup_missing_returns_none() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.pack_by_name("nonexistent").unwrap().is_none());
}

#[test]
fn pack_create_duplicate_name_errors() {
    let mut store = Store::open_in_memory().unwrap();
    store.create_pack("Dup", None).unwrap();
    assert!(
        store.create_pack("Dup", None).is_err(),
        "duplicate name must fail the UNIQUE constraint"
    );
}

#[test]
fn pack_add_paths_and_list() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Tax", Some("tax docs")).unwrap();
    store
        .add_pack_paths(
            &id,
            &[
                "/docs/tax/2024.pdf".to_owned(),
                "/docs/tax/2025.pdf".to_owned(),
            ],
        )
        .unwrap();

    let paths = store.pack_paths(&id).unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&"/docs/tax/2024.pdf".to_owned()));
    assert!(paths.contains(&"/docs/tax/2025.pdf".to_owned()));

    // list_packs reflects the count
    let packs = store.list_packs().unwrap();
    let rec = packs.iter().find(|p| p.name == "Tax").unwrap();
    assert_eq!(rec.path_count, 2);
}

#[test]
fn pack_add_paths_is_idempotent() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Idem", None).unwrap();
    let path = "/a/b.txt".to_owned();
    store
        .add_pack_paths(&id, std::slice::from_ref(&path))
        .unwrap();
    store
        .add_pack_paths(&id, std::slice::from_ref(&path))
        .unwrap(); // must not error or double-count
    assert_eq!(store.pack_paths(&id).unwrap().len(), 1);
}

#[test]
fn pack_remove_paths() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Rem", None).unwrap();
    store
        .add_pack_paths(
            &id,
            &[
                "/x/a.txt".to_owned(),
                "/x/b.txt".to_owned(),
                "/x/c.txt".to_owned(),
            ],
        )
        .unwrap();
    store
        .remove_pack_paths(&id, &["/x/b.txt".to_owned()])
        .unwrap();

    let paths = store.pack_paths(&id).unwrap();
    assert_eq!(paths.len(), 2);
    assert!(!paths.contains(&"/x/b.txt".to_owned()));
}

#[test]
fn pack_remove_nonexistent_path_is_harmless() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Safe", None).unwrap();
    store
        .add_pack_paths(&id, &["/real.txt".to_owned()])
        .unwrap();
    // Removing a path that is not in the pack must not error.
    store
        .remove_pack_paths(&id, &["/ghost.txt".to_owned()])
        .unwrap();
    assert_eq!(store.pack_paths(&id).unwrap().len(), 1);
}

#[test]
fn pack_list_ordered_by_name() {
    let mut store = Store::open_in_memory().unwrap();
    store.create_pack("Zebra", None).unwrap();
    store.create_pack("Alpha", None).unwrap();
    store.create_pack("Mango", None).unwrap();

    let names: Vec<_> = store
        .list_packs()
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["Alpha", "Mango", "Zebra"]);
}

#[test]
fn pack_delete_removes_pack_and_paths() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Gone", None).unwrap();
    store
        .add_pack_paths(&id, &["/a.txt".to_owned(), "/b.txt".to_owned()])
        .unwrap();
    assert_eq!(store.pack_paths(&id).unwrap().len(), 2);

    store.delete_pack(&id).unwrap();

    // Pack is gone.
    assert!(store.pack_by_name("Gone").unwrap().is_none());
    // Cascade removed all pack_paths rows.
    assert!(store.pack_paths(&id).unwrap().is_empty());
    // list_packs returns nothing.
    assert!(store.list_packs().unwrap().is_empty());
}

#[test]
fn pack_delete_nonexistent_is_harmless() {
    let mut store = Store::open_in_memory().unwrap();
    store.delete_pack("no-such-id").unwrap();
}

#[test]
fn pack_paths_ordered_alphabetically() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Order", None).unwrap();
    store
        .add_pack_paths(
            &id,
            &[
                "/z.txt".to_owned(),
                "/a.txt".to_owned(),
                "/m.txt".to_owned(),
            ],
        )
        .unwrap();

    let paths = store.pack_paths(&id).unwrap();
    assert_eq!(paths, vec!["/a.txt", "/m.txt", "/z.txt"]);
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

// ── Importance weights (v0.8) ─────────────────────────────────────────────────

fn hit(path: &str, score: f64) -> SearchHit {
    SearchHit {
        chunk_id: 1,
        entry_path: path.to_owned(),
        seq: 0,
        heading: String::new(),
        text: String::new(),
        rrf_score: score,
    }
}

#[test]
fn weight_set_and_resolve_exact_file() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .set_weight("file", "/a/b.txt", 2.5, "user", None)
        .unwrap();
    assert!((store.weight_for("/a/b.txt").unwrap() - 2.5).abs() < 1e-6);
    // Unknown path → neutral 1.0.
    assert!((store.weight_for("/x/y.txt").unwrap() - 1.0).abs() < 1e-6);
}

#[test]
fn weight_for_uses_nearest_ancestor_dir() {
    let mut store = Store::open_in_memory().unwrap();
    store.set_weight("dir", "/proj", 0.5, "user", None).unwrap();
    store
        .set_weight("dir", "/proj/active", 3.0, "user", None)
        .unwrap();
    // Deepest matching ancestor wins.
    assert!((store.weight_for("/proj/active/main.rs").unwrap() - 3.0).abs() < 1e-6);
    // Falls back to the shallower dir for siblings.
    assert!((store.weight_for("/proj/old/legacy.rs").unwrap() - 0.5).abs() < 1e-6);
}

#[test]
fn weight_for_falls_back_to_category() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_auto_classifications(&[(
            "/docs/tax.pdf".into(),
            "file".into(),
            "finance".into(),
            0.9,
        )])
        .unwrap();
    store
        .set_weight("category", "finance", 4.0, "user", None)
        .unwrap();
    // No file/dir weight → category weight applies.
    assert!((store.weight_for("/docs/tax.pdf").unwrap() - 4.0).abs() < 1e-6);
}

#[test]
fn weight_set_is_upsert() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .set_weight("file", "/a.txt", 2.0, "user", None)
        .unwrap();
    store
        .set_weight("file", "/a.txt", 5.0, "user", None)
        .unwrap();
    assert!((store.weight_for("/a.txt").unwrap() - 5.0).abs() < 1e-6);
    assert_eq!(store.list_weights(None).unwrap().len(), 1);
}

#[test]
fn weight_list_and_delete() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .set_weight("file", "/a.txt", 2.0, "user", None)
        .unwrap();
    store.set_weight("dir", "/proj", 1.5, "user", None).unwrap();
    assert_eq!(store.list_weights(None).unwrap().len(), 2);
    assert_eq!(store.list_weights(Some("file")).unwrap().len(), 1);

    store.delete_weight("file", "/a.txt").unwrap();
    assert_eq!(store.list_weights(None).unwrap().len(), 1);
}

#[test]
fn boost_with_weights_multiplies_and_is_noop_when_empty() {
    let mut store = Store::open_in_memory().unwrap();
    // No weights → unchanged scores.
    let mut hits = vec![hit("/a.txt", 1.0), hit("/b.txt", 2.0)];
    store.boost_with_weights(&mut hits).unwrap();
    assert!((hits[0].rrf_score - 1.0).abs() < 1e-9);
    assert!((hits[1].rrf_score - 2.0).abs() < 1e-9);

    // Boost /a.txt 3x, suppress everything under /arch to 0.1.
    store
        .set_weight("file", "/a.txt", 3.0, "user", None)
        .unwrap();
    store.set_weight("dir", "/arch", 0.1, "user", None).unwrap();
    let mut hits = vec![hit("/a.txt", 1.0), hit("/arch/old.txt", 2.0)];
    store.boost_with_weights(&mut hits).unwrap();
    // Tolerance 1e-6 (not tighter): weights are stored as f32, so 2.0 * 0.1f32 carries
    // ~3e-9 of representation error once widened to f64.
    assert!(
        (hits[0].rrf_score - 3.0).abs() < 1e-6,
        "file weight applied"
    );
    assert!(
        (hits[1].rrf_score - 0.2).abs() < 1e-6,
        "ancestor dir weight applied"
    );
}

#[test]
fn suggest_weights_by_recency_tiers_by_age() {
    let mut store = Store::open_in_memory().unwrap();
    // dummy_entry sets modified=None; insert then patch modified_s to a recent value.
    store
        .upsert_entries(&[dummy_entry("/recent.txt", EntryKind::File, 10)])
        .unwrap();
    let now: i64 = store
        .db_connection()
        .query_row("SELECT unixepoch()", [], |r| r.get(0))
        .unwrap();
    store
        .db_connection()
        .execute(
            "UPDATE entries SET modified_s = ?1 WHERE path = '/recent.txt'",
            [now - 2 * 86400],
        )
        .unwrap();
    let suggestions = store.suggest_weights_by_recency(30).unwrap();
    assert_eq!(suggestions.len(), 1);
    // Modified 2 days ago → top tier weight 2.0.
    assert!((suggestions[0].1 - 2.0).abs() < 1e-6);
}

// ── Insights (v0.10) ──────────────────────────────────────────────────────────

#[test]
fn find_exact_duplicates_groups_by_source_hash() {
    let mut store = Store::open_in_memory().unwrap();
    let mut a = dummy_summary("/a.txt", "file", Some("/"), 1);
    a.source_hash = "HASH1".to_owned();
    let mut b = dummy_summary("/dup/a.txt", "file", Some("/dup"), 2);
    b.source_hash = "HASH1".to_owned();
    let mut c = dummy_summary("/unique.txt", "file", Some("/"), 1);
    c.source_hash = "HASH2".to_owned();
    store.upsert_summary(&a).unwrap();
    store.upsert_summary(&b).unwrap();
    store.upsert_summary(&c).unwrap();

    let clusters = store.find_exact_duplicates().unwrap();
    assert_eq!(clusters.len(), 1, "only HASH1 has 2 members");
    assert_eq!(clusters[0].paths.len(), 2);
    assert!(clusters[0].exact);
}

#[test]
fn find_near_duplicates_clusters_similar_embeddings() {
    let mut store = Store::open_in_memory().unwrap();
    let mut a = dummy_summary("/a.txt", "file", Some("/"), 1);
    a.embedding = Some(vec![1.0, 0.0, 0.0]);
    let mut b = dummy_summary("/b.txt", "file", Some("/"), 1);
    b.embedding = Some(vec![1.0, 0.0, 0.0]); // identical → cosine 1.0
    let mut c = dummy_summary("/c.txt", "file", Some("/"), 1);
    c.embedding = Some(vec![0.0, 1.0, 0.0]); // orthogonal → not in cluster
    store.upsert_summary(&a).unwrap();
    store.upsert_summary(&b).unwrap();
    store.upsert_summary(&c).unwrap();

    let clusters = store.find_near_duplicates(0.9).unwrap();
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].paths, vec!["/a.txt", "/b.txt"]);
    assert!(!clusters[0].exact);
}

#[test]
fn find_stale_entries_returns_old_dirs() {
    let mut store = Store::open_in_memory().unwrap();
    // dummy_entry dirs have modified_s = NULL → counted as stale.
    store
        .upsert_entries(&[
            dummy_entry("/old/proj", EntryKind::Dir, 0),
            dummy_entry("/old/proj/file.txt", EntryKind::File, 10),
        ])
        .unwrap();
    let stale = store.find_stale_entries(365).unwrap();
    // Only the dir is reported (files excluded).
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].path, "/old/proj");
    assert_eq!(stale[0].kind, "dir");
}

#[test]
fn weekly_diff_reports_newly_added() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/new.txt", EntryKind::File, 10)])
        .unwrap();
    let now: i64 = store
        .db_connection()
        .query_row("SELECT unixepoch()", [], |r| r.get(0))
        .unwrap();
    // Window covers the just-inserted entry (first_indexed_at = now).
    let diff = store.weekly_diff(now - 7 * 86400).unwrap();
    assert_eq!(diff.added_count, 1);
    assert!(diff.added.contains(&"/new.txt".to_owned()));
}

// ── Signature graph (v0.18) ───────────────────────────────────────────────────

#[test]
fn code_graph_links_callers_to_definers() {
    let mut store = Store::open_in_memory().unwrap();
    // /app.rs calls `run` and `parse`; /lib.rs defines `run`; /util.rs defines `parse`.
    // /other.rs is outside the scope prefix and must be excluded.
    store
        .upsert_edges(&[
            edge("/src/app.rs", "calls", "run"),
            edge("/src/app.rs", "calls", "parse"),
            edge("/src/lib.rs", "defines", "run"),
            edge("/src/util.rs", "defines", "parse"),
            edge("/other/x.rs", "calls", "run"),
        ])
        .unwrap();

    let g = store.code_graph("/src", 400).unwrap();
    assert!(!g.truncated);
    // Two edges: app→lib (run), app→util (parse). /other excluded by scope.
    assert_eq!(g.edges.len(), 2);
    assert!(g
        .edges
        .iter()
        .any(|e| e.from == "/src/app.rs" && e.to == "/src/lib.rs" && e.weight == 1));
    assert!(g
        .edges
        .iter()
        .any(|e| e.from == "/src/app.rs" && e.to == "/src/util.rs" && e.weight == 1));

    // Node degrees: app out=2 in=0; lib in=1; util in=1.
    let app = g.nodes.iter().find(|n| n.path == "/src/app.rs").unwrap();
    assert_eq!((app.out_degree, app.in_degree), (2, 0));
    let lib = g.nodes.iter().find(|n| n.path == "/src/lib.rs").unwrap();
    assert_eq!((lib.out_degree, lib.in_degree), (0, 1));
}

#[test]
fn code_graph_pagerank_ranks_hub_highest() {
    let mut store = Store::open_in_memory().unwrap();
    // app, lib, util all call into /src/core.rs (the hub); app also calls lib.
    store
        .upsert_edges(&[
            edge("/src/app.rs", "calls", "core_fn"),
            edge("/src/lib.rs", "calls", "core_fn"),
            edge("/src/util.rs", "calls", "core_fn"),
            edge("/src/core.rs", "defines", "core_fn"),
            edge("/src/app.rs", "calls", "lib_fn"),
            edge("/src/lib.rs", "defines", "lib_fn"),
        ])
        .unwrap();

    let g = store.code_graph("/src", 400).unwrap();
    // Centrality is a proper distribution (sums to ~1) over the 4 nodes …
    let sum: f64 = g.nodes.iter().map(|n| n.pagerank).sum();
    assert!((sum - 1.0).abs() < 1e-6, "pagerank sum = {sum}");
    // … and the hub everyone calls into is the most central.
    let top = g
        .nodes
        .iter()
        .max_by(|a, b| a.pagerank.partial_cmp(&b.pagerank).unwrap())
        .unwrap();
    assert_eq!(top.path, "/src/core.rs", "hub should rank highest");
}

#[test]
fn code_graph_weight_counts_shared_symbols_and_excludes_self() {
    let mut store = Store::open_in_memory().unwrap();
    // /a.rs calls two symbols both defined in /b.rs → weight 2.
    // /a.rs also defines and calls `helper` itself → self-edge excluded.
    store
        .upsert_edges(&[
            edge("/a.rs", "calls", "foo"),
            edge("/a.rs", "calls", "bar"),
            edge("/a.rs", "calls", "helper"),
            edge("/a.rs", "defines", "helper"),
            edge("/b.rs", "defines", "foo"),
            edge("/b.rs", "defines", "bar"),
        ])
        .unwrap();

    let g = store.code_graph("/", 400).unwrap();
    assert_eq!(g.edges.len(), 1, "only a→b (self-edge excluded)");
    assert_eq!(g.edges[0].from, "/a.rs");
    assert_eq!(g.edges[0].to, "/b.rs");
    assert_eq!(g.edges[0].weight, 2, "foo + bar shared");
}

#[test]
fn code_graph_truncates_at_cap() {
    let mut store = Store::open_in_memory().unwrap();
    // 3 distinct caller→callee edges; cap at 2 → truncated.
    store
        .upsert_edges(&[
            edge("/a.rs", "calls", "s1"),
            edge("/b.rs", "calls", "s2"),
            edge("/c.rs", "calls", "s3"),
            edge("/d.rs", "defines", "s1"),
            edge("/d.rs", "defines", "s2"),
            edge("/d.rs", "defines", "s3"),
        ])
        .unwrap();
    let g = store.code_graph("/", 2).unwrap();
    assert_eq!(g.edges.len(), 2);
    assert!(g.truncated);
}

#[test]
fn code_graph_excludes_over_common_symbols() {
    let mut store = Store::open_in_memory().unwrap();
    // `gen` is defined in 30 files (> the 25-file cap) → a generic name, excluded.
    // `special` is defined in 1 file → kept.
    let mut edges = Vec::new();
    for i in 0..30 {
        edges.push(edge(&format!("/def{i}.rs"), "defines", "gen"));
    }
    edges.push(edge("/special.rs", "defines", "special"));
    edges.push(edge("/caller.rs", "calls", "gen"));
    edges.push(edge("/caller.rs", "calls", "special"));
    store.upsert_edges(&edges).unwrap();

    let g = store.code_graph("/", 400).unwrap();
    // Only the `special` edge survives; the 30 `gen` edges are filtered as noise.
    assert!(g.edges.iter().all(|e| e.to == "/special.rs"));
    assert_eq!(g.edges.len(), 1);
}

#[test]
fn code_graph_scope_excludes_prefix_siblings() {
    let mut store = Store::open_in_memory().unwrap();
    // "/proj" must NOT match "/projector" (trailing-slash normalization).
    store
        .upsert_edges(&[
            edge("/proj/a.rs", "calls", "run"),
            edge("/proj/b.rs", "defines", "run"),
            edge("/projector/x.rs", "calls", "run"),
            edge("/projector/y.rs", "defines", "run"),
        ])
        .unwrap();
    let g = store.code_graph("/proj", 400).unwrap();
    assert_eq!(g.edges.len(), 1);
    assert_eq!(g.edges[0].from, "/proj/a.rs");
    assert_eq!(g.edges[0].to, "/proj/b.rs");
}

// ── Entry-cleanup integrity contract ──────────────────────────────────────────
// These lock the invariant that deleting an entry leaves NO orphaned rows in any
// entry-keyed child table. There is intentionally no FK ON DELETE CASCADE (the model
// allows entry-less chunks/summaries — deep/summarize without scan), so this manual
// contract is the integrity guarantee. Add any new entry-keyed child table here.

/// Count rows still referencing `path` across every child table that delete_entry /
/// delete_subtree are responsible for clearing. importance_weights is deliberately
/// excluded (weights persist across entry removal by design).
fn orphan_rows_for(store: &Store, path: &str) -> i64 {
    let c = store.db_connection();
    let q = |sql: &str| -> i64 { c.query_row(sql, [path], |r| r.get(0)).unwrap() };
    q("SELECT COUNT(*) FROM chunks WHERE entry_path = ?1")
        + q("SELECT COUNT(*) FROM chunks_fts WHERE entry_path = ?1")
        + q("SELECT COUNT(*) FROM edges WHERE from_path = ?1")
        + q("SELECT COUNT(*) FROM summaries WHERE path = ?1")
        + q("SELECT COUNT(*) FROM summary_queue WHERE path = ?1")
        + q("SELECT COUNT(*) FROM classifications WHERE path = ?1")
}

/// Populate every entry-keyed child table for `path`, then return the store.
fn seed_full_entry(store: &mut Store, path: &str) {
    store
        .upsert_entries(&[dummy_entry(path, EntryKind::File, 100)])
        .unwrap();
    store
        .upsert_chunks(&[dummy_chunk(path, 0, "hello world")])
        .unwrap();
    store
        .upsert_summary(&dummy_summary(path, "file", Some("/"), 1))
        .unwrap();
    store
        .upsert_edges(&[
            edge(path, "imports", "std::fs"),
            edge(path, "defines", "run"),
        ])
        .unwrap();
    store
        .enqueue_summary_items(&[(path.to_owned(), "file".to_owned(), 1)])
        .unwrap();
    store
        .upsert_auto_classifications(&[(path.into(), "file".into(), "code".into(), 0.9)])
        .unwrap();
}

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
