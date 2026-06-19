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
