use super::*;

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

#[cfg(unix)]
#[test]
fn open_hardens_db_and_dir_perms() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    let _store = Store::open(&db).unwrap();
    // The DB holds the indexed corpus (incl. secrets) — file 0600, data dir 0700, so other
    // local users on a shared host can't read it.
    let fmode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
    assert_eq!(fmode, 0o600, "db file should be 0600, got {fmode:o}");
    let dmode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(dmode, 0o700, "data dir should be 0700, got {dmode:o}");
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
fn paths_modified_since_filters_by_mtime_and_kind() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            entry_with_mtime("/r/new.rs", EntryKind::File, 1000),
            entry_with_mtime("/r/old.rs", EntryKind::File, 100),
            entry_with_mtime("/r/recentdir", EntryKind::Dir, 1000), // dirs excluded
            dummy_entry("/r/nomtime.rs", EntryKind::File, 5),       // NULL mtime excluded
        ])
        .unwrap();

    let recent = store.paths_modified_since(500).unwrap();
    assert!(recent.contains(&"/r/new.rs".to_owned()), "recent file kept");
    assert!(
        !recent.contains(&"/r/old.rs".to_owned()),
        "file older than cutoff excluded"
    );
    assert!(
        !recent.iter().any(|p| p == "/r/recentdir"),
        "directories excluded (no content mtime)"
    );
    assert!(
        !recent.iter().any(|p| p == "/r/nomtime.rs"),
        "NULL-mtime entry excluded (can't claim it changed)"
    );
}

#[test]
fn cosine_search_matches_brute_force_oracle() {
    // D2: the allocation-free `cosine_search` must return byte-identical top-k to the old
    // brute-force scan. Compare against an independent oracle over the same data.
    let mut store = Store::open_in_memory().unwrap();
    let dim = 8usize;
    let n = 60usize;
    let k = 12usize;

    // Deterministic pseudo-random f32s in [-0.5, 0.5) — reproducible, no `rand` dep.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };

    let mut entries = Vec::new();
    let mut chunks = Vec::new();
    let mut embeds: Vec<(String, Vec<f32>)> = Vec::new();
    for i in 0..n {
        let path = format!("/corpus/f{i:03}.rs");
        entries.push(dummy_entry(&path, EntryKind::File, 100));
        let emb: Vec<f32> = (0..dim).map(|_| next()).collect();
        chunks.push(ChunkRecord {
            embedding: Some(emb.clone()),
            embed_model: Some("test".to_owned()),
            ..dummy_chunk(&path, 0, "some indexable text")
        });
        embeds.push((path, emb));
    }
    store.upsert_entries(&entries).unwrap();
    store.upsert_chunks(&chunks).unwrap();

    let query: Vec<f32> = (0..dim).map(|_| next()).collect();
    let got = store.cosine_search(&query, k, None).unwrap();

    // Oracle: brute-force cosine, STABLE sort by score DESC (insertion order == id order, so ties
    // resolve exactly as `cosine_search`'s id-ascending tie-break).
    let qnorm: f32 = query.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mut oracle: Vec<(String, f32)> = embeds
        .iter()
        .map(|(p, e)| {
            let dot: f32 = query.iter().zip(e).map(|(x, y)| x * y).sum();
            let en: f32 = e.iter().map(|x| x * x).sum::<f32>().sqrt();
            let sim = if qnorm == 0.0 || en == 0.0 {
                0.0
            } else {
                dot / (qnorm * en)
            };
            (p.clone(), sim)
        })
        .collect();
    oracle.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let got_paths: Vec<&str> = got.iter().map(|(_, p)| p.as_str()).collect();
    let oracle_paths: Vec<&str> = oracle.iter().take(k).map(|(p, _)| p.as_str()).collect();
    assert_eq!(got.len(), k, "returns exactly top-k");
    assert_eq!(
        got_paths, oracle_paths,
        "top-k order matches the brute-force oracle"
    );
}

#[test]
fn cosine_search_handles_empty_limit0_and_scope() {
    let mut store = Store::open_in_memory().unwrap();
    assert!(store
        .cosine_search(&[0.1, 0.2, 0.3], 5, None)
        .unwrap()
        .is_empty());
    store
        .upsert_entries(&[dummy_entry("/a/x.rs", EntryKind::File, 1)])
        .unwrap();
    store
        .upsert_chunks(&[ChunkRecord {
            embedding: Some(vec![1.0, 0.0, 0.0]),
            embed_model: Some("t".to_owned()),
            ..dummy_chunk("/a/x.rs", 0, "text")
        }])
        .unwrap();
    assert!(store
        .cosine_search(&[1.0, 0.0, 0.0], 0, None)
        .unwrap()
        .is_empty());
    let hit = store.cosine_search(&[1.0, 0.0, 0.0], 5, None).unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].1, "/a/x.rs");
    assert!(store
        .cosine_search(&[1.0, 0.0, 0.0], 5, Some("/b"))
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .cosine_search(&[1.0, 0.0, 0.0], 5, Some("/a"))
            .unwrap()
            .len(),
        1
    );
}
