use super::*;

#[test]
fn upsert_and_read_back_by_anchor() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_note_anchor("/notes/a.md", "parse", "symbol", "Parsing gotcha", "eng")
        .unwrap();
    let found = store.note_anchors_for("parse").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title, "Parsing gotcha");
    assert_eq!(found[0].anchor_kind, "symbol");
    assert_eq!(found[0].pack, "eng");

    assert!(store.note_anchors_for("nothing-here").unwrap().is_empty());
}

#[test]
fn upsert_is_replace_not_append() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_note_anchor("/notes/a.md", "old_anchor", "symbol", "T1", "eng")
        .unwrap();
    store
        .upsert_note_anchor("/notes/a.md", "new_anchor", "path", "T2", "eng")
        .unwrap();
    assert!(store.note_anchors_for("old_anchor").unwrap().is_empty());
    let found = store.note_anchors_for("new_anchor").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title, "T2");
}

#[test]
fn multiple_notes_can_anchor_the_same_target() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_note_anchor("/notes/a.md", "parse", "symbol", "T1", "eng")
        .unwrap();
    store
        .upsert_note_anchor("/notes/b.md", "parse", "symbol", "T2", "eng")
        .unwrap();
    let found = store.note_anchors_for("parse").unwrap();
    assert_eq!(found.len(), 2);
}

#[test]
fn anchors_are_cleared_when_their_note_entry_is_deleted() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[dummy_entry("/notes/a.md", EntryKind::File, 10)])
        .unwrap();
    store
        .upsert_note_anchor("/notes/a.md", "parse", "symbol", "T", "eng")
        .unwrap();
    store.delete_entry("/notes/a.md").unwrap();
    assert!(store.note_anchors_for("parse").unwrap().is_empty());
}
