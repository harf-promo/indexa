use super::*;

fn app(path: &str, kind: &str, name: &str, spec: u32, primary: bool) -> DetectedApp {
    DetectedApp {
        path: path.to_owned(),
        app_kind: kind.to_owned(),
        app_name: name.to_owned(),
        family: "code".to_owned(),
        specificity: spec,
        is_primary: primary,
        markers_json: "[]".to_owned(),
        source: "builtin".to_owned(),
        detected_at: 0,
    }
}

#[test]
fn replace_and_read_back_apps_for_dir() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_apps_for_dir(
            "/proj/web",
            &[
                app(
                    "/proj/web",
                    "node_package",
                    "Node.js / npm package",
                    10,
                    false,
                ),
                app("/proj/web", "nextjs_app", "Next.js app", 30, true),
            ],
        )
        .unwrap();

    let apps = store.apps_for_dir("/proj/web").unwrap();
    assert_eq!(apps.len(), 2);
    // Primary first.
    assert_eq!(apps[0].app_kind, "nextjs_app");
    assert!(apps[0].is_primary);

    let primary = store.primary_app_for_dir("/proj/web").unwrap().unwrap();
    assert_eq!(primary.app_kind, "nextjs_app");
}

#[test]
fn replace_is_idempotent_not_append() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_apps_for_dir("/p", &[app("/p", "rust_crate", "Rust crate", 10, true)])
        .unwrap();
    // Re-detect: the folder is now only a Go module — old row must be gone, not accumulated.
    store
        .replace_apps_for_dir("/p", &[app("/p", "go_module", "Go module", 10, true)])
        .unwrap();
    let apps = store.apps_for_dir("/p").unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].app_kind, "go_module");
}

#[test]
fn no_apps_yields_empty_and_none() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.apps_for_dir("/nope").unwrap().is_empty());
    assert!(store.primary_app_for_dir("/nope").unwrap().is_none());
}

#[test]
fn primary_apps_under_returns_one_per_dir_in_subtree() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_apps_for_dir(
            "/repo/api",
            &[
                app("/repo/api", "django_app", "Django project", 30, true),
                app("/repo/api", "python_requirements", "Python", 10, false),
            ],
        )
        .unwrap();
    store
        .replace_apps_for_dir(
            "/repo/web",
            &[app("/repo/web", "nextjs_app", "Next.js app", 30, true)],
        )
        .unwrap();
    store
        .replace_apps_for_dir(
            "/other",
            &[app("/other", "rust_crate", "Rust crate", 10, true)],
        )
        .unwrap();

    let under = store.primary_apps_under("/repo").unwrap();
    // Only primaries, only under /repo: one per dir (Django + Next.js), not the secondary Python.
    let kinds: Vec<&str> = under.iter().map(|a| a.app_kind.as_str()).collect();
    assert_eq!(kinds, vec!["django_app", "nextjs_app"]);
}

#[test]
fn top_projects_under_drops_nested_and_vendored() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_apps_for_dir(
            "/dev/indexa",
            &[app("/dev/indexa", "rust_crate", "Rust crate", 10, true)],
        )
        .unwrap();
    store
        .replace_apps_for_dir(
            "/dev/indexa/crates/core",
            &[app(
                "/dev/indexa/crates/core",
                "rust_crate",
                "Rust crate",
                10,
                true,
            )],
        )
        .unwrap();
    store
        .replace_apps_for_dir(
            "/dev/site/assets/vendor/jquery",
            &[app(
                "/dev/site/assets/vendor/jquery",
                "node_package",
                "Node.js / npm package",
                10,
                true,
            )],
        )
        .unwrap();
    store
        .replace_apps_for_dir(
            "/dev/site",
            &[app("/dev/site", "nextjs_app", "Next.js app", 30, true)],
        )
        .unwrap();

    let top = store.top_projects_under("/dev").unwrap();
    let paths: Vec<&str> = top.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths, vec!["/dev/indexa", "/dev/site"]);
}

#[test]
fn path_coverage_counts_chunks_and_summary() {
    use crate::store::types::ChunkRecord;
    use crate::walker::{Entry, EntryKind};
    use std::path::PathBuf;

    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_entries(&[
            Entry {
                path: PathBuf::from("/p"),
                kind: EntryKind::Dir,
                size: 0,
                modified: None,
                hint: None,
                is_binary: false,
            },
            Entry {
                path: PathBuf::from("/p/a.rs"),
                kind: EntryKind::File,
                size: 10,
                modified: None,
                hint: None,
                is_binary: false,
            },
        ])
        .unwrap();
    store
        .upsert_chunks(&[ChunkRecord {
            entry_path: "/p/a.rs".into(),
            seq: 0,
            heading: String::new(),
            text: "fn main() {}".into(),
            language: Some("rust".into()),
            embedding: None,
            embed_model: None,
            content_hash: None,
        }])
        .unwrap();

    let empty = store.path_coverage("/p").unwrap();
    assert_eq!(empty.chunk_count, 1);
    assert!(!empty.has_summary);
    assert_eq!(empty.total, 1); // the dir itself

    store.upsert_summary(&file_summary_for("/p")).unwrap();
    let built = store.path_coverage("/p").unwrap();
    assert!(built.has_summary);
    assert_eq!(built.chunk_count, 1);
}

fn file_summary_for(path: &str) -> crate::store::SummaryRecord {
    crate::store::SummaryRecord {
        path: path.to_owned(),
        kind: "dir".into(),
        parent_path: None,
        depth: 0,
        summary: "a project".into(),
        summary_l0: None,
        embedding: None,
        child_count: 1,
        byte_size: 10,
        model: "test".into(),
        source_hash: "H".into(),
        generated_at: 1,
    }
}

#[test]
fn is_project_noise_path_matches_vendor_and_xcodeproj() {
    assert!(is_project_noise_path("/a/assets/vendor/foo"));
    assert!(is_project_noise_path("/app/ios/Runner.xcodeproj"));
    assert!(!is_project_noise_path("/a/Barrq/mobile"));
}
