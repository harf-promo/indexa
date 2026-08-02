use super::*;
use crate::store::ComputedModule;

fn module(id: usize, label: &str, cohesion: f64, members: &[&str]) -> ComputedModule {
    ComputedModule {
        module_id: id,
        label: label.to_owned(),
        cohesion,
        members: members.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn replace_graph_modules_round_trips_labels_and_members() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_graph_modules(&[
            module(0, "auth", 1.0, &["/a/one.rs", "/a/two.rs"]),
            module(1, "storage", 0.5, &["/b/three.rs"]),
        ])
        .unwrap();
    let modules = store.graph_modules().unwrap();
    assert_eq!(modules.len(), 2);
    let auth = modules.iter().find(|m| m.label == "auth").unwrap();
    assert_eq!(auth.cohesion, 1.0);
    assert_eq!(
        auth.members,
        vec!["/a/one.rs".to_owned(), "/a/two.rs".to_owned()]
    );
}

#[test]
fn replace_graph_modules_clears_the_prior_set() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_graph_modules(&[module(0, "old", 1.0, &["/a.rs"])])
        .unwrap();
    store
        .replace_graph_modules(&[module(0, "new", 1.0, &["/b.rs"])])
        .unwrap();
    let modules = store.graph_modules().unwrap();
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].label, "new");
    assert_eq!(modules[0].members, vec!["/b.rs".to_owned()]);
}

#[test]
fn graph_modules_for_scope_returns_only_modules_with_a_matching_member_but_keeps_full_membership() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_graph_modules(&[
            module(0, "a", 1.0, &["/repo/a/one.rs", "/repo/a/two.rs"]),
            module(1, "b", 1.0, &["/repo/b/three.rs"]),
        ])
        .unwrap();
    let scoped = store.graph_modules_for_scope("/repo/a").unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].label, "a");
    assert_eq!(
        scoped[0].members.len(),
        2,
        "full membership even under a scoped query"
    );
}

#[test]
fn graph_modules_empty_when_never_computed() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.graph_modules().unwrap().is_empty());
}
