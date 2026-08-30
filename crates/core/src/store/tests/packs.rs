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

#[test]
fn migrates_legacy_pack_events_check_preserving_all_rows() {
    // Pre-G2b indexes had `pack_events.event CHECK IN ('created','path_added','path_removed',
    // 'renamed','exported')`. Store::open must widen the CHECK to include 'refreshed' AND
    // preserve every legacy row — the table-recreate must not silently drop rows (the reason
    // the copy is a plain explicit INSERT, not INSERT OR IGNORE). Mirrors
    // `migrates_legacy_edges_check_preserving_all_rows` in store::tests::graph.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pre_refreshed.db");
    {
        // Minimal legacy packs/pack_events schema with the OLD 5-value CHECK + a seeded row.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE packs (
                 id          TEXT PRIMARY KEY,
                 name        TEXT NOT NULL UNIQUE,
                 description TEXT,
                 created_at  INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE pack_events (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 pack_id TEXT NOT NULL REFERENCES packs(id) ON DELETE CASCADE,
                 event   TEXT NOT NULL
                             CHECK(event IN ('created','path_added','path_removed','renamed','exported')),
                 detail  TEXT,
                 at      INTEGER NOT NULL DEFAULT (unixepoch())
             );
             INSERT INTO packs (id, name) VALUES ('p1', 'proj');
             INSERT INTO pack_events (pack_id, event, detail) VALUES
                 ('p1', 'created', 'proj'),
                 ('p1', 'exported', 'xml');",
        )
        .unwrap();
    }

    // Store::open runs the CHECK-widening migration.
    let mut store = Store::open(&path).expect("must open & migrate a pre-'refreshed' index");

    // 1) Row parity: both legacy rows survive.
    let events = store.pack_events("p1").unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(kinds, vec!["created", "exported"]);

    // 2) The widened CHECK actually accepts 'refreshed' now.
    store
        .record_pack_refreshed("p1", "1 reindexed, 0 vanished")
        .expect("widened CHECK must accept 'refreshed'");
    let events = store.pack_events("p1").unwrap();
    assert_eq!(events.last().unwrap().event, "refreshed");

    // 3) The rebuilt table's `REFERENCES packs(id) ON DELETE CASCADE` still works post-migration
    // — the copy-table migration recreates the FK declaration too, not just the CHECK; a typo
    // there would silently orphan events on delete instead of erroring. Mirrors
    // `deleting_a_pack_cascades_its_events`.
    store.delete_pack("p1").unwrap();
    assert!(
        store.pack_events("p1").unwrap().is_empty(),
        "ON DELETE CASCADE must survive the pack_events table rebuild"
    );
}

#[test]
fn record_pack_refreshed_appends_a_refreshed_event() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    store
        .record_pack_refreshed(&id, "2 reindexed, 1 vanished (left flagged)")
        .unwrap();
    let events = store.pack_events(&id).unwrap();
    assert_eq!(events.last().unwrap().event, "refreshed");
    assert_eq!(
        events.last().unwrap().detail.as_deref(),
        Some("2 reindexed, 1 vanished (left flagged)")
    );
}

#[test]
fn create_pack_records_a_created_event() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    let events = store.pack_events(&id).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "created");
    assert_eq!(events[0].detail.as_deref(), Some("proj"));
}

#[test]
fn pack_events_are_chronological_across_the_crud_lifecycle() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    store
        .add_pack_paths(&id, &["/a.rs".to_owned(), "/b.rs".to_owned()])
        .unwrap();
    store.rename_pack(&id, "proj2").unwrap();
    store.remove_pack_paths(&id, &["/a.rs".to_owned()]).unwrap();
    store.record_pack_exported(&id, "xml").unwrap();

    let events = store.pack_events(&id).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "created",
            "path_added",
            "renamed",
            "path_removed",
            "exported"
        ]
    );
    assert_eq!(events[1].detail.as_deref(), Some("/a.rs, /b.rs"));
    assert_eq!(events[2].detail.as_deref(), Some("proj2"));
    assert_eq!(events[4].detail.as_deref(), Some("xml"));
}

#[test]
fn add_and_remove_pack_paths_with_empty_batch_records_no_event() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    store.add_pack_paths(&id, &[]).unwrap();
    store.remove_pack_paths(&id, &[]).unwrap();
    let events = store.pack_events(&id).unwrap();
    assert_eq!(events.len(), 1, "only the initial 'created' event");
}

#[test]
fn rename_pack_no_op_records_no_event() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    // Renaming a nonexistent pack id changes 0 rows -> no event.
    store.rename_pack("nonexistent-id", "whatever").unwrap();
    let events = store.pack_events(&id).unwrap();
    assert_eq!(events.len(), 1);
}

#[test]
fn pack_updated_at_reflects_the_latest_event() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    let created = store.pack_by_name("proj").unwrap().unwrap();
    assert!(created.updated_at.is_some());

    store.add_pack_paths(&id, &["/a.rs".to_owned()]).unwrap();
    let after_add = store.pack_by_name("proj").unwrap().unwrap();
    assert!(after_add.updated_at.unwrap() >= created.updated_at.unwrap());
}

#[test]
fn deleting_a_pack_cascades_its_events() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.create_pack("proj", None).unwrap();
    store.add_pack_paths(&id, &["/a.rs".to_owned()]).unwrap();
    assert_eq!(store.pack_events(&id).unwrap().len(), 2);

    store.delete_pack(&id).unwrap();
    assert!(store.pack_events(&id).unwrap().is_empty());
}

// ── Per-item inclusion mode (v0.78) ─────────────────────────────────────────────

#[test]
fn new_pack_items_default_to_reference_inclusion_mode() {
    // A freshly-added item, on a brand-new (already-migrated) DB, must default to
    // "reference" — the export behavior every pack item had before this field existed
    // (build fresh from the current summaries tree at export time), not "pinned".
    let mut store = Store::open_in_memory().unwrap();
    let pid = store.create_pack("proj", None).unwrap();
    store.add_pack_paths(&pid, &["/a.rs".to_owned()]).unwrap();

    let items = store.pack_item_records(&pid).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, "/a.rs");
    assert_eq!(items[0].inclusion_mode, "reference");
    assert!(items[0].pinned_snapshot.is_none());
}

#[test]
fn pinning_a_pack_item_captures_its_indexed_chunks() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[
            dummy_chunk("/a.rs", 0, "fn one() {}"),
            dummy_chunk("/a.rs", 1, "fn two() {}"),
        ])
        .unwrap();
    let pid = store.create_pack("proj", None).unwrap();
    store.add_pack_paths(&pid, &["/a.rs".to_owned()]).unwrap();

    let n = store
        .set_pack_item_inclusion_mode(&pid, "/a.rs", "pinned")
        .unwrap();
    assert_eq!(n, 1);

    let items = store.pack_item_records(&pid).unwrap();
    assert_eq!(items[0].inclusion_mode, "pinned");
    let snapshot = items[0].pinned_snapshot.as_deref().unwrap();
    assert!(snapshot.contains("fn one() {}"), "got: {snapshot}");
    assert!(snapshot.contains("fn two() {}"), "got: {snapshot}");

    // Switching back to "reference" clears the frozen snapshot — it must not linger as
    // invisible dead weight once the item no longer reads from it.
    store
        .set_pack_item_inclusion_mode(&pid, "/a.rs", "reference")
        .unwrap();
    let items = store.pack_item_records(&pid).unwrap();
    assert_eq!(items[0].inclusion_mode, "reference");
    assert!(items[0].pinned_snapshot.is_none());
}

#[test]
fn pinning_a_directory_member_captures_its_whole_indexed_subtree() {
    // Same member→indexed-file prefix expansion `stale_pack_paths` uses (subtree_match),
    // exercised here for the snapshot capture instead of the staleness check.
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[dummy_chunk("/proj/src/a.rs", 0, "mod a;")])
        .unwrap();
    let pid = store.create_pack("proj", None).unwrap();
    store
        .add_pack_paths(&pid, &["/proj/src".to_owned()])
        .unwrap();

    store
        .set_pack_item_inclusion_mode(&pid, "/proj/src", "pinned")
        .unwrap();
    let items = store.pack_item_records(&pid).unwrap();
    let snapshot = items[0].pinned_snapshot.as_deref().unwrap();
    assert!(snapshot.contains("mod a;"), "got: {snapshot}");
}

#[test]
fn pinning_an_unindexed_item_leaves_snapshot_none() {
    // Pinning is valid even when nothing has been indexed for that path yet — it just
    // captures nothing (rather than erroring), consistent with "pin now, backfill later".
    let mut store = Store::open_in_memory().unwrap();
    let pid = store.create_pack("proj", None).unwrap();
    store
        .add_pack_paths(&pid, &["/never-indexed.rs".to_owned()])
        .unwrap();

    store
        .set_pack_item_inclusion_mode(&pid, "/never-indexed.rs", "pinned")
        .unwrap();
    let items = store.pack_item_records(&pid).unwrap();
    assert_eq!(items[0].inclusion_mode, "pinned");
    assert!(items[0].pinned_snapshot.is_none());
}

#[test]
fn set_pack_item_inclusion_mode_rejects_unknown_mode() {
    let mut store = Store::open_in_memory().unwrap();
    let pid = store.create_pack("proj", None).unwrap();
    store.add_pack_paths(&pid, &["/a.rs".to_owned()]).unwrap();
    let err = store
        .set_pack_item_inclusion_mode(&pid, "/a.rs", "bogus")
        .unwrap_err();
    assert!(err.to_string().contains("invalid inclusion mode"));
}

#[test]
fn set_pack_item_inclusion_mode_on_nonexistent_path_changes_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    let pid = store.create_pack("proj", None).unwrap();
    let n = store
        .set_pack_item_inclusion_mode(&pid, "/never-added.rs", "pinned")
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn migrates_legacy_pack_paths_adding_inclusion_mode_preserving_all_rows() {
    // Pre-v0.78 indexes have no inclusion_mode/pinned_snapshot columns on pack_paths.
    // Store::open must add both AND preserve every legacy row, defaulting each to
    // "reference" — the export behavior those rows already had.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pre_inclusion_mode.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE packs (
                 id          TEXT PRIMARY KEY,
                 name        TEXT NOT NULL UNIQUE,
                 description TEXT,
                 created_at  INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE pack_paths (
                 pack_id  TEXT NOT NULL REFERENCES packs(id) ON DELETE CASCADE,
                 path     TEXT NOT NULL,
                 added_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 PRIMARY KEY (pack_id, path)
             );
             INSERT INTO packs (id, name) VALUES ('p1', 'proj');
             INSERT INTO pack_paths (pack_id, path) VALUES ('p1', '/legacy.rs');",
        )
        .unwrap();
    }

    let mut store = Store::open(&path).expect("must open & migrate a pre-inclusion-mode index");
    let items = store.pack_item_records("p1").unwrap();
    assert_eq!(items.len(), 1, "the legacy row must survive the migration");
    assert_eq!(items[0].path, "/legacy.rs");
    assert_eq!(
        items[0].inclusion_mode, "reference",
        "a legacy row must default to 'reference', matching its pre-migration export behavior"
    );
    assert!(items[0].pinned_snapshot.is_none());

    // The migrated column is fully usable, not just present.
    store
        .set_pack_item_inclusion_mode("p1", "/legacy.rs", "pinned")
        .unwrap();
    assert_eq!(
        store.pack_item_records("p1").unwrap()[0].inclusion_mode,
        "pinned"
    );
}
