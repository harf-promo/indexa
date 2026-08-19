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
fn graph_modules_for_scope_excludes_prefix_siblings() {
    // H4: "/proj" must NOT match "/projector" — a scoped module-list query used to leak a
    // sibling directory's modules purely because it shared the string prefix.
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_graph_modules(&[
            module(0, "proj-mod", 1.0, &["/proj/a.rs"]),
            module(1, "projector-mod", 1.0, &["/projector/b.rs"]),
        ])
        .unwrap();
    let scoped = store.graph_modules_for_scope("/proj").unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].label, "proj-mod");
}

#[test]
fn graph_modules_for_scope_empty_prefix_still_means_all() {
    // graph_modules() itself calls graph_modules_for_scope(""), relying on an empty prefix
    // meaning "no scope restriction" — subtree_match_or_all must preserve that, not narrow
    // it to a literal empty-string path match (which no row has).
    let mut store = Store::open_in_memory().unwrap();
    store
        .replace_graph_modules(&[
            module(0, "a", 1.0, &["/repo/a.rs"]),
            module(1, "b", 1.0, &["/repo/b.rs"]),
        ])
        .unwrap();
    assert_eq!(store.graph_modules_for_scope("").unwrap().len(), 2);
}

#[test]
fn graph_modules_empty_when_never_computed() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.graph_modules().unwrap().is_empty());
}
