use super::*;

// ── Scoped call resolution (v0.25) ───────────────────────────────────────────

#[test]
fn scoped_same_file_definition_stops_repo_wide_fanout() {
    let mut store = Store::open_in_memory().unwrap();
    // The killer case: /src/a/caller.rs has its OWN `parse` helper. Bare matching
    // linked it to every other `parse` definer; same-file resolution binds it locally
    // and produces no cross-file edge at all.
    store
        .upsert_edges(&[
            edge("/src/a/caller.rs", "defines", "parse"),
            edge("/src/a/caller.rs", "calls", "parse"),
            edge("/src/b/lib.rs", "defines", "parse"),
            edge("/src/b/user.rs", "calls", "parse"),
        ])
        .unwrap();

    let g = store.code_graph_scoped("/src", 400, false).unwrap();
    assert!(
        !g.graph.edges.iter().any(|e| e.from == "/src/a/caller.rs"),
        "a caller with its own definition must not fan out: {:?}",
        g.graph.edges
    );
    // user.rs resolves same-dir to lib.rs only — not to caller.rs across the repo.
    assert_eq!(g.graph.edges.len(), 1);
    assert_eq!(g.graph.edges[0].from, "/src/b/user.rs");
    assert_eq!(g.graph.edges[0].to, "/src/b/lib.rs");
    assert_eq!(g.edge_tiers[0], ResolutionTier::SameDir);
}

#[test]
fn scoped_same_dir_narrows_definers() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "f"),
            edge("/p/b.rs", "defines", "f"),
            edge("/q/c.rs", "defines", "f"),
        ])
        .unwrap();
    let g = store.code_graph_scoped("/", 400, false).unwrap();
    assert_eq!(
        g.graph.edges.len(),
        1,
        "same-dir definer wins over cross-dir"
    );
    assert_eq!(g.graph.edges[0].to, "/p/b.rs");
    assert_eq!(g.edge_tiers[0], ResolutionTier::SameDir);
}

#[test]
fn scoped_import_resolves_js_relative_specifier() {
    let mut store = Store::open_in_memory().unwrap();
    // Two files define `parse`; the caller imports exactly one of them ('./lib/parse',
    // extensionless) → exactly one target, import tier.
    store
        .upsert_edges(&[
            edge("/app/src/main.ts", "calls", "parse"),
            edge("/app/src/main.ts", "imports", "./lib/parse"),
            edge("/app/src/lib/parse.ts", "defines", "parse"),
            edge("/other/parse.py", "defines", "parse"),
        ])
        .unwrap();
    let g = store.code_graph_scoped("/", 400, false).unwrap();
    assert_eq!(g.graph.edges.len(), 1, "import match must pick one target");
    assert_eq!(g.graph.edges[0].to, "/app/src/lib/parse.ts");
    assert_eq!(g.edge_tiers[0], ResolutionTier::Import);
}

#[test]
fn scoped_import_resolves_rust_crate_and_super_paths() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            // crate:: form, with a brace group (`use crate::util::{helpers}` records
            // "crate::util::{helpers}") plus an item path needing the minus-last try.
            edge("/repo/src/cli.rs", "calls", "helper_fn"),
            edge(
                "/repo/src/cli.rs",
                "imports",
                "crate::util::helpers::helper_fn",
            ),
            edge("/repo/src/util/helpers.rs", "defines", "helper_fn"),
            edge("/elsewhere/src/helpers.rs", "defines", "helper_fn"),
            // super::super:: form from a nested module.
            edge("/r/src/m/deep/a.rs", "calls", "u_fn"),
            edge("/r/src/m/deep/a.rs", "imports", "super::super::util"),
            edge("/r/src/m/util.rs", "defines", "u_fn"),
            edge("/r/x/util.rs", "defines", "u_fn"),
        ])
        .unwrap();
    let g = store.code_graph_scoped("/", 400, false).unwrap();
    let find = |from: &str| {
        g.graph
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from == from)
            .map(|(i, e)| (e.to.clone(), g.edge_tiers[i]))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        find("/repo/src/cli.rs"),
        vec![(
            "/repo/src/util/helpers.rs".to_owned(),
            ResolutionTier::Import
        )],
        "crate:: path must resolve within the caller's crate root only"
    );
    assert_eq!(
        find("/r/src/m/deep/a.rs"),
        vec![("/r/src/m/util.rs".to_owned(), ResolutionTier::Import)],
        "super::super:: must climb exactly one extra directory"
    );
}

#[test]
fn scoped_import_resolves_python_dotted_module() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/proj/app/main.py", "calls", "parse_doc"),
            edge("/proj/app/main.py", "imports", "pkg.parser"),
            edge("/proj/pkg/parser.py", "defines", "parse_doc"),
            edge("/misc/tools.py", "defines", "parse_doc"),
            // package __init__ form
            edge("/proj/app/boot.py", "calls", "init_app"),
            edge("/proj/app/boot.py", "imports", "pkg"),
            edge("/proj/pkg/__init__.py", "defines", "init_app"),
            edge("/misc/extra.py", "defines", "init_app"),
        ])
        .unwrap();
    let g = store.code_graph_scoped("/", 400, false).unwrap();
    let to_of = |from: &str| {
        g.graph
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from == from)
            .map(|(i, e)| (e.to.clone(), g.edge_tiers[i]))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        to_of("/proj/app/main.py"),
        vec![("/proj/pkg/parser.py".to_owned(), ResolutionTier::Import)]
    );
    assert_eq!(
        to_of("/proj/app/boot.py"),
        vec![("/proj/pkg/__init__.py".to_owned(), ResolutionTier::Import)]
    );
}

#[test]
fn who_calls_resolved_reports_tiers_and_targets() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            // same-file: defines its own `parse`
            edge("/a/own.rs", "defines", "parse"),
            edge("/a/own.rs", "calls", "parse"),
            // same-dir: definer next to the caller
            edge("/p/caller.rs", "calls", "parse"),
            edge("/p/def.rs", "defines", "parse"),
            // import: TS relative
            edge("/j/main.ts", "calls", "parse"),
            edge("/j/main.ts", "imports", "./lib/parse"),
            edge("/j/lib/parse.ts", "defines", "parse"),
            // bare: no structural link
            edge("/z/far.go", "calls", "parse"),
        ])
        .unwrap();

    let resolved = store.who_calls_resolved("parse", 100).unwrap();
    let by_path: std::collections::HashMap<&str, &ResolvedCaller> =
        resolved.iter().map(|r| (r.path.as_str(), r)).collect();

    let own = by_path["/a/own.rs"];
    assert_eq!(own.tier, ResolutionTier::SameFile);
    assert_eq!(own.targets, vec!["/a/own.rs".to_owned()]);

    let neighbor = by_path["/p/caller.rs"];
    assert_eq!(neighbor.tier, ResolutionTier::SameDir);
    assert_eq!(neighbor.targets, vec!["/p/def.rs".to_owned()]);

    let imported = by_path["/j/main.ts"];
    assert_eq!(imported.tier, ResolutionTier::Import);
    assert_eq!(imported.targets, vec!["/j/lib/parse.ts".to_owned()]);

    let far = by_path["/z/far.go"];
    assert_eq!(far.tier, ResolutionTier::Bare);
    assert!(far.targets.is_empty(), "bare callers carry no target list");
}

#[test]
fn related_files_resolved_drops_cross_noise_and_keeps_tiers() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            // /app.py defines its own `parse` AND calls it → the other definer must
            // NOT become "related" through that symbol (old bare join's cross-noise).
            edge("/x/app.py", "defines", "parse"),
            edge("/x/app.py", "calls", "parse"),
            edge("/y/other.py", "defines", "parse"),
            // genuine dependency via import
            edge("/x/app.py", "calls", "load_cfg"),
            edge("/x/app.py", "imports", "cfg.loader"),
            edge("/cfg/loader.py", "defines", "load_cfg"),
            // genuine dependent: same-dir caller of app's export
            edge("/x/app.py", "defines", "boot"),
            edge("/x/cli.py", "calls", "boot"),
        ])
        .unwrap();
    let related = store.find_related_files_resolved("/x/app.py", 10).unwrap();
    let paths: Vec<(&str, ResolutionTier)> =
        related.iter().map(|r| (r.path.as_str(), r.tier)).collect();
    assert!(
        !paths.iter().any(|(p, _)| *p == "/y/other.py"),
        "self-defined symbol must not relate to its repo-wide name twins: {paths:?}"
    );
    assert!(paths.contains(&("/cfg/loader.py", ResolutionTier::Import)));
    assert!(paths.contains(&("/x/cli.py", ResolutionTier::SameDir)));
}

/// Integration gate: a realistic mini-repo (15 files; Rust + TS + Python + Go import
/// shapes) seeded straight into the edges table. Asserts (a) the exact scoped edge set,
/// (b) measured precision: 6 scoped edges vs 11 bare-name edges (5 false positives
/// dropped), and (c) **zero lost true edges** — every same-file/same-dir/import-confirmed
/// link bare matching found is still present; only cross-noise is gone.
#[test]
fn scoped_mini_repo_fixture_improves_precision_without_losing_true_edges() {
    let mut store = Store::open_in_memory().unwrap();
    let fixture = [
        // Rust app crate: main.rs imports a submodule; decoy definer in another crate.
        edge("/mr/crates/app/src/main.rs", "calls", "run_engine"),
        edge(
            "/mr/crates/app/src/main.rs",
            "imports",
            "crate::engine::core",
        ),
        edge("/mr/crates/app/src/engine/core.rs", "defines", "run_engine"),
        edge("/mr/crates/zzz/src/core.rs", "defines", "run_engine"),
        // Rust same-file helper named like the TS parser (killer case).
        edge("/mr/crates/app/src/fmt.rs", "defines", "parse"),
        edge("/mr/crates/app/src/fmt.rs", "calls", "parse"),
        // TS app: relative import of the real parser.
        edge("/mr/ts/src/main.ts", "calls", "parse"),
        edge("/mr/ts/src/main.ts", "imports", "./lib/parse"),
        edge("/mr/ts/src/lib/parse.ts", "defines", "parse"),
        // Python app: dotted module import; decoy definer elsewhere.
        edge("/mr/py/app/main.py", "calls", "parse_doc"),
        edge("/mr/py/app/main.py", "imports", "pkg.parsing"),
        edge("/mr/py/pkg/parsing.py", "defines", "parse_doc"),
        edge("/mr/misc/tools.py", "defines", "parse_doc"),
        // Go service: same-dir definer; decoy JS definer elsewhere (Go imports are
        // package paths and deliberately don't resolve — same-dir still does).
        edge("/mr/go/svc/handler.go", "calls", "render"),
        edge("/mr/go/svc/render.go", "defines", "render"),
        edge("/mr/web/render.js", "defines", "render"),
        // Unresolvable: two cross-dir definers, no imports → stays bare (labeled).
        edge("/mr/tools/runner.rs", "calls", "execute"),
        edge("/mr/lib1/exec.rs", "defines", "execute"),
        edge("/mr/lib2/exec2.py", "defines", "execute"),
    ];
    store.upsert_edges(&fixture).unwrap();

    // Bare-name baseline, derived from the same fixture: every (caller, definer) pair
    // sharing a symbol name, minus self-pairs — what the pre-v0.25 join produced.
    let mut bare_pairs: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    for c in fixture.iter().filter(|e| e.kind == "calls") {
        for d in fixture
            .iter()
            .filter(|e| e.kind == "defines" && e.to_ref == c.to_ref)
        {
            if c.from_path != d.from_path {
                bare_pairs.insert((c.from_path.clone(), d.from_path.clone()));
            }
        }
    }
    assert_eq!(bare_pairs.len(), 11, "bare baseline edge count");

    let g = store.code_graph_scoped("/mr", 400, false).unwrap();
    let scoped_pairs: std::collections::BTreeSet<(String, String)> = g
        .graph
        .edges
        .iter()
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();

    // (a) exact scoped edge set: 4 resolved + 2 bare fallback = 6.
    let expected: std::collections::BTreeSet<(String, String)> = [
        (
            "/mr/crates/app/src/main.rs",
            "/mr/crates/app/src/engine/core.rs",
        ),
        ("/mr/ts/src/main.ts", "/mr/ts/src/lib/parse.ts"),
        ("/mr/py/app/main.py", "/mr/py/pkg/parsing.py"),
        ("/mr/go/svc/handler.go", "/mr/go/svc/render.go"),
        ("/mr/tools/runner.rs", "/mr/lib1/exec.rs"),
        ("/mr/tools/runner.rs", "/mr/lib2/exec2.py"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
    .collect();
    assert_eq!(scoped_pairs, expected);

    // (b) measured precision shift: 11 bare → 6 scoped (5 cross-noise edges dropped,
    // including BOTH fan-outs from the self-defined `parse` helper).
    assert_eq!((bare_pairs.len(), scoped_pairs.len()), (11, 6));
    assert!(
        !scoped_pairs
            .iter()
            .any(|(f, _)| f == "/mr/crates/app/src/fmt.rs"),
        "self-defined helper must produce no out-edges"
    );

    // (c) zero lost true edges: scoped ⊆ bare, and every resolved-tier edge bare
    // found is still present (only name-coincidence noise was dropped).
    assert!(
        scoped_pairs.is_subset(&bare_pairs),
        "scoped resolution must never invent edges"
    );
    let tier_count = |t: ResolutionTier| g.edge_tiers.iter().filter(|x| **x == t).count();
    assert_eq!(tier_count(ResolutionTier::Import), 3, "rust + ts + py");
    assert_eq!(tier_count(ResolutionTier::SameDir), 1, "go same-dir");
    assert_eq!(tier_count(ResolutionTier::Bare), 2, "labeled fallback");
    assert_eq!(tier_count(ResolutionTier::SameFile), 0, "never cross-file");

    // Strict composes: bare tier gone, resolved edges untouched.
    let strict = store.code_graph_scoped("/mr", 400, true).unwrap();
    assert_eq!(strict.graph.edges.len(), 4);
    assert!(strict.edge_tiers.iter().all(|t| *t != ResolutionTier::Bare));
}

#[test]
fn code_graph_scope_excludes_prefix_siblings() {
    let mut store = Store::open_in_memory().unwrap();
    // "/proj" must NOT match "/projector" (trailing-slash normalization).
    store
        .upsert_edges(&[
            edge("/proj/a.rs", "calls", "run"),
            edge("/proj/b.rs", "defines", "run"),
            edge("/projector/x.rs", "calls", "run"),
            edge("/projector/y.rs", "defines", "run"),
        ])
        .unwrap();
    let g = store.code_graph("/proj", 400, false).unwrap();
    assert_eq!(g.edges.len(), 1);
    assert_eq!(g.edges[0].from, "/proj/a.rs");
    assert_eq!(g.edges[0].to, "/proj/b.rs");
}
