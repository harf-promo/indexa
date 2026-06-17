use super::*;

// ── Importance weights (v0.8) ─────────────────────────────────────────────────

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
