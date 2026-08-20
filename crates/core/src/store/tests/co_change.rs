use super::*;

#[test]
fn replace_co_change_stores_pairs_regardless_of_input_order() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_co_change(&[
            ("/b.rs".into(), "/a.rs".into(), 5), // reverse lexical order on input
        ])
        .unwrap();
    let from_a = store.co_change_for("/a.rs", 10).unwrap();
    let from_b = store.co_change_for("/b.rs", 10).unwrap();
    assert_eq!(
        from_a,
        vec![CoChangePair {
            path: "/b.rs".into(),
            count: 5
        }]
    );
    assert_eq!(
        from_b,
        vec![CoChangePair {
            path: "/a.rs".into(),
            count: 5
        }]
    );
}

#[test]
fn replace_co_change_clears_the_prior_set() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_co_change(&[("/a.rs".into(), "/b.rs".into(), 3)])
        .unwrap();
    store
        .replace_co_change(&[("/c.rs".into(), "/d.rs".into(), 1)])
        .unwrap();
    assert!(store.co_change_for("/a.rs", 10).unwrap().is_empty());
    assert_eq!(store.co_change_for("/c.rs", 10).unwrap().len(), 1);
}

#[test]
fn replace_co_change_skips_self_pairs() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_co_change(&[("/a.rs".into(), "/a.rs".into(), 9)])
        .unwrap();
    assert!(store.co_change_for("/a.rs", 10).unwrap().is_empty());
}

#[test]
fn co_change_for_orders_heaviest_first_and_respects_limit() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_co_change(&[
            ("/a.rs".into(), "/x.rs".into(), 1),
            ("/a.rs".into(), "/y.rs".into(), 9),
            ("/a.rs".into(), "/z.rs".into(), 5),
        ])
        .unwrap();
    let top2 = store.co_change_for("/a.rs", 2).unwrap();
    assert_eq!(
        top2.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        vec!["/y.rs", "/z.rs"]
    );
}

#[test]
fn co_change_rows_are_cleared_when_either_file_is_deleted() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/a.rs", EntryKind::File, 10),
            dummy_entry("/b.rs", EntryKind::File, 10),
        ])
        .unwrap();
    store
        .replace_co_change(&[("/a.rs".into(), "/b.rs".into(), 4)])
        .unwrap();
    store.delete_entry("/b.rs").unwrap();
    assert!(store.co_change_for("/a.rs", 10).unwrap().is_empty());
}
