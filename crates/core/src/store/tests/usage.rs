use super::*;

// ── Token-savings telemetry (store::usage) ────────────────────────────────────

#[test]
fn tool_usage_record_and_weekly_summary() {
    let mut store = Store::open_in_memory().unwrap();

    // Empty index: zero aggregate, and no savings line to print.
    let empty = store.usage_summary(USAGE_WEEK_SECS).unwrap();
    assert_eq!(empty.calls, 0);
    assert!(empty.savings_line().is_none());

    store
        .record_tool_usage("mcp", "search", 100, 4_000)
        .unwrap();
    store.record_tool_usage("cli", "ask", 50, 1_000).unwrap();

    let u = store.usage_summary(USAGE_WEEK_SECS).unwrap();
    assert_eq!(u.calls, 2);
    assert_eq!(u.bytes_served, 150);
    assert_eq!(u.bytes_counterfactual, 5_000);

    // (5000 - 150) / 4 = 1212 tokens; the line must carry the ≈ caveat.
    let line = u.savings_line().unwrap();
    assert!(line.contains("roughly 1.2K tokens saved"), "line: {line}");
    assert!(line.contains("≈4 bytes/token"), "line: {line}");
}

#[test]
fn usage_summary_window_excludes_old_rows_and_gc_removes_them() {
    let mut store = Store::open_in_memory().unwrap();
    store.record_tool_usage("web", "ask", 10, 100).unwrap();
    store.record_tool_usage("web", "ask", 20, 200).unwrap();
    // Age one row past the weekly window (8 days).
    store
        .db_connection()
        .execute_batch("UPDATE tool_usage SET at = at - 691200 WHERE id = 1")
        .unwrap();

    let week = store.usage_summary(USAGE_WEEK_SECS).unwrap();
    assert_eq!(week.calls, 1);
    assert_eq!(week.bytes_served, 20);

    // A wide-enough window still sees both rows; GC then drops the aged one.
    let all = store.usage_summary(USAGE_WEEK_SECS * 100).unwrap();
    assert_eq!(all.calls, 2);
    assert_eq!(store.gc_usage(USAGE_WEEK_SECS).unwrap(), 1);
    assert_eq!(store.usage_summary(USAGE_WEEK_SECS * 100).unwrap().calls, 1);
}

#[test]
fn counterfactual_dedups_paths_and_falls_back_to_summary_byte_size() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            dummy_entry("/p/a.rs", EntryKind::File, 1_234),
            dummy_entry("/p", EntryKind::Dir, 0),
        ])
        .unwrap();
    // Dir entry has size 0 → must fall back to summaries.byte_size (100 in
    // dummy_summary — the subtree total a client would otherwise read).
    store
        .upsert_summary(&dummy_summary("/p", "dir", None, 0))
        .unwrap();

    // /p/a.rs counted ONCE despite two hits; unknown path contributes 0.
    let total = store
        .counterfactual_bytes_for_paths(&["/p/a.rs", "/p/a.rs", "/p", "/nope.txt"])
        .unwrap();
    assert_eq!(total, 1_234 + 100);
}
