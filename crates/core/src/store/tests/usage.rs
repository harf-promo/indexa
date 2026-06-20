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
        .record_tool_usage("mcp", "search", 100, 4_000, None)
        .unwrap();
    store
        .record_tool_usage("cli", "ask", 50, 1_000, None)
        .unwrap();

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
    store
        .record_tool_usage("web", "ask", 10, 100, None)
        .unwrap();
    store
        .record_tool_usage("web", "ask", 20, 200, None)
        .unwrap();
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

#[test]
fn session_usage_ledger_sums_per_session_and_ignores_stateless() {
    let mut store = Store::open_in_memory().unwrap();
    // Two conversational sessions + one stateless (None) call.
    store
        .record_tool_usage("web", "ask", 100, 1_000, Some("sess-1"))
        .unwrap();
    store
        .record_tool_usage("web", "ask", 50, 800, Some("sess-1"))
        .unwrap();
    store
        .record_tool_usage("web", "ask", 30, 500, Some("sess-2"))
        .unwrap();
    store
        .record_tool_usage("cli", "ask", 25, 250, None)
        .unwrap();

    // Per-session ledger sums only that session's rows (all-time, not windowed).
    let s1 = store.session_usage_summary("sess-1").unwrap();
    assert_eq!(s1.calls, 2);
    assert_eq!(s1.bytes_served, 150);
    assert_eq!(s1.bytes_counterfactual, 1_800);

    let s2 = store.session_usage_summary("sess-2").unwrap();
    assert_eq!(s2.calls, 1);
    assert_eq!(s2.bytes_counterfactual, 500);

    // Unknown session → zero, never an error.
    assert_eq!(store.session_usage_summary("nope").unwrap().calls, 0);

    // Every call (incl. the stateless None one) still lands in the weekly aggregate.
    assert_eq!(store.usage_summary(USAGE_WEEK_SECS).unwrap().calls, 4);
}
