//! Conversational Ask session store tests.

use super::*;

#[test]
fn append_and_recent_turns_round_trip() {
    let mut store = Store::open_in_memory().unwrap();
    store.ensure_session("s1", Some("docs/")).unwrap();
    assert_eq!(store.append_turn("s1", "q0", "a0", "[]").unwrap(), 0);
    assert_eq!(store.append_turn("s1", "q1", "a1", "[]").unwrap(), 1);
    assert_eq!(store.append_turn("s1", "q2", "a2", "[]").unwrap(), 2);

    let all = store.turns_for_session("s1").unwrap();
    assert_eq!(all.len(), 3);
    // Chronological order, monotonic index.
    assert_eq!(all[0].turn_index, 0);
    assert_eq!(all[0].question, "q0");
    assert_eq!(all[2].turn_index, 2);
    assert_eq!(all[2].answer, "a2");
}

#[test]
fn recent_turns_clamps_and_orders_chronologically() {
    let mut store = Store::open_in_memory().unwrap();
    store.ensure_session("s1", None).unwrap();
    for i in 0..5 {
        store
            .append_turn("s1", &format!("q{i}"), &format!("a{i}"), "[]")
            .unwrap();
    }
    // The last two turns, oldest-first.
    let recent = store.recent_turns("s1", 2).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].question, "q3");
    assert_eq!(recent[1].question, "q4");

    // Asking for more than exist returns all of them.
    assert_eq!(store.recent_turns("s1", 99).unwrap().len(), 5);
    // Unknown session → empty, not an error.
    assert!(store.recent_turns("nope", 5).unwrap().is_empty());
}

#[test]
fn ensure_session_is_idempotent() {
    let mut store = Store::open_in_memory().unwrap();
    store.ensure_session("s1", Some("a/")).unwrap();
    assert!(store.session_exists("s1").unwrap());
    let created: i64 = store
        .db_connection()
        .query_row(
            "SELECT created_at FROM ask_sessions WHERE id='s1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Re-ensure with a different scope must not reset the row.
    store.ensure_session("s1", Some("b/")).unwrap();
    let created2: i64 = store
        .db_connection()
        .query_row(
            "SELECT created_at FROM ask_sessions WHERE id='s1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created, created2);
    let scope: String = store
        .db_connection()
        .query_row("SELECT scope FROM ask_sessions WHERE id='s1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(scope, "a/", "scope is set on create and not overwritten");
    assert!(!store.session_exists("missing").unwrap());
}

#[test]
fn deleting_session_cascades_to_turns() {
    let mut store = Store::open_in_memory().unwrap();
    store.ensure_session("s1", None).unwrap();
    store.append_turn("s1", "q", "a", "[]").unwrap();
    store.append_turn("s1", "q2", "a2", "[]").unwrap();
    store
        .db_connection()
        .execute("DELETE FROM ask_sessions WHERE id='s1'", [])
        .unwrap();
    let remaining: i64 = store
        .db_connection()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turns WHERE session_id='s1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "ON DELETE CASCADE removed the turns");
}
