use super::*;

// ── Insights (v0.10) ──────────────────────────────────────────────────────────

#[test]
fn all_summaries_and_all_edges_for_snapshot() {
    let mut store = Store::open_in_memory().unwrap();
    let mut s = dummy_summary("/r", "dir", Some("/"), 0);
    s.embedding = Some(vec![0.1, 0.2, 0.3]);
    store.upsert_summary(&s).unwrap();
    store
        .upsert_summary(&dummy_summary("/r/a", "file", Some("/r"), 1))
        .unwrap();
    store
        .upsert_edges(&[edge("/r/a", "defines", "foo"), edge("/r/a", "calls", "bar")])
        .unwrap();

    let summaries = store.all_summaries().unwrap();
    assert_eq!(summaries.len(), 2);
    // Embeddings are intentionally omitted from the bulk getter (snapshot size).
    assert!(summaries.iter().all(|s| s.embedding.is_none()));
    assert_eq!(store.all_edges().unwrap().len(), 2);
}

#[test]
fn saved_queries_crud_roundtrip() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .save_query("prio", "what are my priorities?", "rrf", None)
        .unwrap();
    store
        .save_query("auth", "how does auth work?", "agentic", Some("/src/auth"))
        .unwrap();
    let q = store.get_saved_query("auth").unwrap().unwrap();
    assert_eq!(q.question, "how does auth work?");
    assert_eq!(q.mode, "agentic");
    assert_eq!(q.scope.as_deref(), Some("/src/auth"));
    let all = store.list_saved_queries().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name, "auth"); // alphabetical
                                     // Overwrite by name.
    store
        .save_query("prio", "updated?", "sparse", None)
        .unwrap();
    assert_eq!(
        store.get_saved_query("prio").unwrap().unwrap().mode,
        "sparse"
    );
    assert_eq!(store.list_saved_queries().unwrap().len(), 2);
    // Delete.
    assert_eq!(store.delete_saved_query("prio").unwrap(), 1);
    assert!(store.get_saved_query("prio").unwrap().is_none());
    assert_eq!(store.delete_saved_query("nope").unwrap(), 0);
}

#[test]
fn find_largest_orders_files_by_size() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/small.txt", EntryKind::File, 100),
            dummy_entry("/big.txt", EntryKind::File, 9000),
            dummy_entry("/mid.txt", EntryKind::File, 500),
            dummy_entry("/adir", EntryKind::Dir, 0), // dirs excluded
        ])
        .unwrap();
    let top = store.find_largest(2).unwrap();
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].path, "/big.txt");
    assert_eq!(top[0].size, 9000);
    assert_eq!(top[1].path, "/mid.txt");
}

#[test]
fn language_breakdown_counts_chunks_per_language() {
    let mut store = Store::open_in_memory().unwrap();
    let mut rust1 = dummy_chunk("/a.rs", 0, "fn a");
    rust1.language = Some("rust".into());
    let mut rust2 = dummy_chunk("/b.rs", 0, "fn b");
    rust2.language = Some("rust".into());
    let mut py = dummy_chunk("/c.py", 0, "def c");
    py.language = Some("python".into());
    let untagged = dummy_chunk("/d.txt", 0, "plain"); // no language → excluded
    store.upsert_chunks(&[rust1, rust2, py, untagged]).unwrap();
    let langs = store.language_breakdown().unwrap();
    assert_eq!(langs.len(), 2);
    assert_eq!(langs[0].language, "rust"); // most chunks first
    assert_eq!(langs[0].chunks, 2);
    assert_eq!(langs[1].language, "python");
    assert_eq!(langs[1].chunks, 1);
}

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
fn near_dup_lsh_matches_exact_clusters_on_seeded_set() {
    use crate::store::insights::{near_dup_clusters_exact, near_dup_clusters_lsh};
    let items = seeded_near_dup_items(&[4, 3, 2], 40, 24, 0xC0FFEE);
    let exact = near_dup_clusters_exact(&items, 0.9);
    let lsh = near_dup_clusters_lsh(&items, 0.9);
    assert_eq!(
        exact.len(),
        3,
        "exact path should find the 3 planted groups"
    );
    // Same clusters: compare order-normalized path sets.
    let norm = |clusters: &[DuplicateCluster]| {
        let mut v: Vec<Vec<String>> = clusters.iter().map(|c| c.paths.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(
        norm(&exact),
        norm(&lsh),
        "LSH clusters must match the exact path on this seeded set"
    );
    for c in &lsh {
        assert!(!c.exact);
        assert!(c.similarity >= 0.9);
    }
}

#[test]
fn near_dup_lsh_is_deterministic_across_runs() {
    use crate::store::insights::near_dup_clusters_lsh;
    let items = seeded_near_dup_items(&[5, 2], 60, 24, 0xDECAF);
    let a = near_dup_clusters_lsh(&items, 0.9);
    let b = near_dup_clusters_lsh(&items, 0.9);
    assert!(!a.is_empty());
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.paths, y.paths);
        // Bitwise-identical averages: candidate order is sorted, not
        // HashSet-iteration order, so float sums must not drift.
        assert_eq!(x.similarity.to_bits(), y.similarity.to_bits());
    }
}

#[test]
fn find_near_duplicates_uses_lsh_above_exact_cap() {
    use crate::store::insights::{SplitMix64, NEAR_DUP_EXACT_MAX};
    let mut store = Store::open_in_memory().unwrap();
    let mut rng = SplitMix64(7);
    // Two planted near-identical files in a sea of random vectors big enough
    // to cross the exact→LSH switchover (the old code silently capped at 5K).
    let dup: Vec<f32> = (0..16).map(|_| rng.next_unit()).collect();
    for path in ["/dup/one.txt", "/dup/two.txt"] {
        let mut s = dummy_summary(path, "file", Some("/dup"), 1);
        s.embedding = Some(dup.iter().map(|x| x + rng.next_unit() * 0.001).collect());
        store.upsert_summary(&s).unwrap();
    }
    for k in 0..(NEAR_DUP_EXACT_MAX + 10) {
        let mut s = dummy_summary(&format!("/sea/file{k:05}.txt"), "file", Some("/sea"), 1);
        s.embedding = Some((0..16).map(|_| rng.next_unit()).collect());
        store.upsert_summary(&s).unwrap();
    }
    let clusters = store.find_near_duplicates(0.95).unwrap();
    let dup_cluster = clusters
        .iter()
        .find(|c| c.paths.contains(&"/dup/one.txt".to_owned()))
        .expect("planted near-dup pair must be found by the LSH path");
    assert!(dup_cluster.paths.contains(&"/dup/two.txt".to_owned()));
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
