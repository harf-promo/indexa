use super::*;

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
fn pack_rename_changes_name_and_preserves_id() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("Auth", None).unwrap();
    let changed = store.rename_pack(&id, "Authentication").unwrap();
    assert_eq!(changed, 1);
    assert!(store.pack_by_name("Auth").unwrap().is_none());
    let rec = store.pack_by_name("Authentication").unwrap().unwrap();
    assert_eq!(rec.id, id, "rename keeps the same pack id");
    // Renaming a non-existent id changes nothing.
    assert_eq!(store.rename_pack("deadbeef", "x").unwrap(), 0);
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

#[test]
fn stale_pack_paths_flags_out_of_date_and_missing_members() {
    // Real files on disk so `stale_pack_paths` can stat their live mtime.
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh.txt");
    let stale = dir.path().join("stale.txt");
    std::fs::write(&fresh, b"fresh content").unwrap();
    std::fs::write(&stale, b"stale content").unwrap();
    let fresh_s = fresh.to_string_lossy().to_string();
    let stale_s = stale.to_string_lossy().to_string();

    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[
            dummy_chunk_embedded(&fresh_s, 0, "fresh content"),
            dummy_chunk_embedded(&stale_s, 0, "stale content"),
        ])
        .unwrap();
    // Pin indexed_at deterministically (no timing race): fresh indexed FAR AFTER its
    // mtime → current; stale indexed at the epoch, long before its mtime → out of date.
    store
        .db_connection()
        .execute(
            "UPDATE chunks SET indexed_at = 4102444800 WHERE entry_path = ?1", // year 2100
            rusqlite::params![fresh_s],
        )
        .unwrap();
    store
        .db_connection()
        .execute(
            "UPDATE chunks SET indexed_at = 1 WHERE entry_path = ?1", // 1970
            rusqlite::params![stale_s],
        )
        .unwrap();

    // Pack references the DIRECTORY, exercising member→indexed-file prefix expansion.
    let pid = store.create_pack("proj", None).unwrap();
    store
        .add_pack_paths(&pid, &[dir.path().to_string_lossy().to_string()])
        .unwrap();
    assert_eq!(
        store.stale_pack_paths(&pid).unwrap(),
        vec![stale_s.clone()],
        "only the file indexed before its current mtime is stale"
    );

    // A pack whose only member is the fresh (current) file has nothing stale.
    let clean = store.create_pack("fresh-only", None).unwrap();
    store
        .add_pack_paths(&clean, std::slice::from_ref(&fresh_s))
        .unwrap();
    assert!(store.stale_pack_paths(&clean).unwrap().is_empty());

    // A member that no longer exists on disk can't be stat'd → counts as stale.
    std::fs::remove_file(&stale).unwrap();
    assert_eq!(store.stale_pack_paths(&pid).unwrap(), vec![stale_s]);
}
