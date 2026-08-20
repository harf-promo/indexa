use super::*;

fn sym(path: &str, name: &str, kind: &str, start: i64, end: i64) -> SymbolRecord {
    SymbolRecord {
        path: path.to_owned(),
        name: name.to_owned(),
        kind: kind.to_owned(),
        start_line: start,
        end_line: end,
    }
}

#[test]
fn upsert_symbols_then_read_back_ordered_by_line() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_symbols(&[
            sym("/p/a.rs", "run", "fn", 10, 15),
            sym("/p/a.rs", "Widget", "struct", 1, 5),
        ])
        .unwrap();
    let syms = store.symbols_in_file("/p/a.rs").unwrap();
    assert_eq!(syms.len(), 2);
    // Ordered by start_line — Widget (1) before run (10).
    assert_eq!(syms[0].name, "Widget");
    assert_eq!(syms[0].kind, "struct");
    assert_eq!(syms[1].name, "run");
    assert_eq!(syms[1].kind, "fn");
}

#[test]
fn upsert_symbols_replaces_a_files_rows_on_re_deep() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_symbols(&[sym("/p/a.rs", "old_fn", "fn", 1, 3)])
        .unwrap();
    // A re-deep with a renamed/removed symbol must not leave the old row behind.
    store
        .upsert_symbols(&[sym("/p/a.rs", "new_fn", "fn", 1, 4)])
        .unwrap();
    let syms = store.symbols_in_file("/p/a.rs").unwrap();
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "new_fn");
}

#[test]
fn upsert_symbols_only_clears_the_files_present_in_the_batch() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_symbols(&[sym("/p/a.rs", "a_fn", "fn", 1, 3)])
        .unwrap();
    store
        .upsert_symbols(&[sym("/p/b.rs", "b_fn", "fn", 1, 3)])
        .unwrap();
    assert_eq!(store.symbols_in_file("/p/a.rs").unwrap().len(), 1);
    assert_eq!(store.symbols_in_file("/p/b.rs").unwrap().len(), 1);
}

#[test]
fn symbols_overlapping_matches_any_line_range_intersection() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_symbols(&[
            sym("/p/a.rs", "before", "fn", 1, 5),
            sym("/p/a.rs", "touched", "fn", 8, 20),
            sym("/p/a.rs", "after", "fn", 30, 35),
        ])
        .unwrap();
    // A hunk spanning lines 10-12 overlaps only "touched" (8..=20).
    let hits = store.symbols_overlapping("/p/a.rs", 10, 12).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "touched");

    // A hunk spanning the boundary (5..=8) overlaps both "before" (ends at 5) and
    // "touched" (starts at 8) — inclusive overlap.
    let boundary = store.symbols_overlapping("/p/a.rs", 5, 8).unwrap();
    assert_eq!(boundary.len(), 2);

    // A hunk entirely outside every symbol's range matches nothing.
    assert!(store
        .symbols_overlapping("/p/a.rs", 100, 110)
        .unwrap()
        .is_empty());
}

#[test]
fn symbols_are_cleared_when_their_entry_is_deleted() {
    // Regression guard: symbols must participate in the same manual cascade-cleanup
    // contract as chunks/edges/summaries (no FK ON DELETE CASCADE in this schema).
    let mut store = Store::open_in_memory().unwrap();
    seed_full_entry(&mut store, "/proj/a.rs");
    assert_eq!(store.symbols_in_file("/proj/a.rs").unwrap().len(), 1);

    store.delete_entry("/proj/a.rs").unwrap();
    assert!(store.symbols_in_file("/proj/a.rs").unwrap().is_empty());
}
