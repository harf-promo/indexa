use super::*;

// ── Signature graph (v0.18) ───────────────────────────────────────────────────

#[test]
fn code_graph_links_callers_to_definers() {
    let mut store = Store::open_in_memory().unwrap();
    // /app.rs calls `run` and `parse`; /lib.rs defines `run`; /util.rs defines `parse`.
    // /other.rs is outside the scope prefix and must be excluded.
    store
        .upsert_edges(&[
            edge("/src/app.rs", "calls", "run"),
            edge("/src/app.rs", "calls", "parse"),
            edge("/src/lib.rs", "defines", "run"),
            edge("/src/util.rs", "defines", "parse"),
            edge("/other/x.rs", "calls", "run"),
        ])
        .unwrap();

    let g = store.code_graph("/src", 400, false).unwrap();
    assert!(!g.truncated);
    // Two edges: app→lib (run), app→util (parse). /other excluded by scope.
    assert_eq!(g.edges.len(), 2);
    assert!(g
        .edges
        .iter()
        .any(|e| e.from == "/src/app.rs" && e.to == "/src/lib.rs" && e.weight == 1));
    assert!(g
        .edges
        .iter()
        .any(|e| e.from == "/src/app.rs" && e.to == "/src/util.rs" && e.weight == 1));

    // Node degrees: app out=2 in=0; lib in=1; util in=1.
    let app = g.nodes.iter().find(|n| n.path == "/src/app.rs").unwrap();
    assert_eq!((app.out_degree, app.in_degree), (2, 0));
    let lib = g.nodes.iter().find(|n| n.path == "/src/lib.rs").unwrap();
    assert_eq!((lib.out_degree, lib.in_degree), (0, 1));
}

#[test]
fn code_graph_pagerank_ranks_hub_highest() {
    let mut store = Store::open_in_memory().unwrap();
    // app, lib, util all call into /src/core.rs (the hub); app also calls lib.
    store
        .upsert_edges(&[
            edge("/src/app.rs", "calls", "core_fn"),
            edge("/src/lib.rs", "calls", "core_fn"),
            edge("/src/util.rs", "calls", "core_fn"),
            edge("/src/core.rs", "defines", "core_fn"),
            edge("/src/app.rs", "calls", "lib_fn"),
            edge("/src/lib.rs", "defines", "lib_fn"),
        ])
        .unwrap();

    let g = store.code_graph("/src", 400, false).unwrap();
    // Centrality is a proper distribution (sums to ~1) over the 4 nodes …
    let sum: f64 = g.nodes.iter().map(|n| n.pagerank).sum();
    assert!((sum - 1.0).abs() < 1e-6, "pagerank sum = {sum}");
    // … and the hub everyone calls into is the most central.
    let top = g
        .nodes
        .iter()
        .max_by(|a, b| a.pagerank.partial_cmp(&b.pagerank).unwrap())
        .unwrap();
    assert_eq!(top.path, "/src/core.rs", "hub should rank highest");
}

#[test]
fn code_graph_weight_counts_shared_symbols_and_excludes_self() {
    let mut store = Store::open_in_memory().unwrap();
    // /a.rs calls two symbols both defined in /b.rs → weight 2.
    // /a.rs also defines and calls `helper` itself → self-edge excluded.
    store
        .upsert_edges(&[
            edge("/a.rs", "calls", "foo"),
            edge("/a.rs", "calls", "bar"),
            edge("/a.rs", "calls", "helper"),
            edge("/a.rs", "defines", "helper"),
            edge("/b.rs", "defines", "foo"),
            edge("/b.rs", "defines", "bar"),
        ])
        .unwrap();

    let g = store.code_graph("/", 400, false).unwrap();
    assert_eq!(g.edges.len(), 1, "only a→b (self-edge excluded)");
    assert_eq!(g.edges[0].from, "/a.rs");
    assert_eq!(g.edges[0].to, "/b.rs");
    assert_eq!(g.edges[0].weight, 2, "foo + bar shared");
}

#[test]
fn code_graph_truncates_at_cap() {
    let mut store = Store::open_in_memory().unwrap();
    // 3 distinct caller→callee edges; cap at 2 → truncated.
    store
        .upsert_edges(&[
            edge("/a.rs", "calls", "s1"),
            edge("/b.rs", "calls", "s2"),
            edge("/c.rs", "calls", "s3"),
            edge("/d.rs", "defines", "s1"),
            edge("/d.rs", "defines", "s2"),
            edge("/d.rs", "defines", "s3"),
        ])
        .unwrap();
    let g = store.code_graph("/", 2, false).unwrap();
    assert_eq!(g.edges.len(), 2);
    assert!(g.truncated);
}

#[test]
fn code_graph_excludes_over_common_symbols() {
    let mut store = Store::open_in_memory().unwrap();
    // `gen` is defined in 30 files (> the 25-file cap) → a generic name, excluded.
    // `special` is defined in 1 file → kept.
    let mut edges = Vec::new();
    for i in 0..30 {
        edges.push(edge(&format!("/def{i}.rs"), "defines", "gen"));
    }
    edges.push(edge("/special.rs", "defines", "special"));
    edges.push(edge("/caller.rs", "calls", "gen"));
    edges.push(edge("/caller.rs", "calls", "special"));
    store.upsert_edges(&edges).unwrap();

    let g = store.code_graph("/", 400, false).unwrap();
    // Only the `special` edge survives; the 30 `gen` edges are filtered as noise.
    assert!(g.edges.iter().all(|e| e.to == "/special.rs"));
    assert_eq!(g.edges.len(), 1);
}

#[test]
fn code_graph_strict_drops_bare_tier_edges() {
    let mut store = Store::open_in_memory().unwrap();
    // `parse` has two definers in OTHER directories with no import link → both edges
    // are bare-tier. `unique` is import-resolved (TS relative specifier). Strict keeps
    // only structurally-resolved edges, so the bare pair vanishes.
    store
        .upsert_edges(&[
            edge("/a/app.ts", "calls", "parse"),
            edge("/a/app.ts", "calls", "unique"),
            edge("/a/app.ts", "imports", "../d/util"),
            edge("/b/p1.rs", "defines", "parse"),
            edge("/c/p2.rs", "defines", "parse"),
            edge("/d/util.ts", "defines", "unique"),
        ])
        .unwrap();

    // Default (scoped): 2 bare `parse` edges + 1 import-resolved `unique` edge.
    let scoped = store.code_graph_scoped("/", 400, false).unwrap();
    assert_eq!(scoped.graph.edges.len(), 3);
    let bare = scoped
        .edge_tiers
        .iter()
        .filter(|t| **t == ResolutionTier::Bare)
        .count();
    assert_eq!(bare, 2, "the two cross-dir parse edges are bare-tier");

    // Strict: bare tier filtered out entirely — only the import-confirmed edge remains.
    let strict = store.code_graph_scoped("/", 400, true).unwrap();
    assert_eq!(strict.graph.edges.len(), 1);
    assert_eq!(strict.graph.edges[0].from, "/a/app.ts");
    assert_eq!(strict.graph.edges[0].to, "/d/util.ts");
    assert_eq!(strict.edge_tiers[0], ResolutionTier::Import);
}

#[test]
fn blast_radius_strict_cuts_bare_transitive_hop() {
    let mut store = Store::open_in_memory().unwrap();
    // target() is called by /a/mid.rs (direct caller), which exports `helper`. /c/far.rs
    // calls `helper` with no structural link to either definer (different dirs, no
    // imports) → bare tier: kept in the default mode (labeled), dropped under strict.
    store
        .upsert_edges(&[
            edge("/a/mid.rs", "calls", "target"),
            edge("/a/mid.rs", "defines", "helper"),
            edge("/b/other.rs", "defines", "helper"),
            edge("/c/far.rs", "calls", "helper"),
        ])
        .unwrap();

    let fuzzy = store
        .blast_radius_resolved("target", 200, false, 2)
        .unwrap();
    assert!(fuzzy.files.contains(&"/a/mid.rs".to_string()));
    assert!(
        fuzzy.files.contains(&"/c/far.rs".to_string()),
        "default mode keeps the bare transitive hop (labeled)"
    );
    assert_eq!((fuzzy.direct, fuzzy.bare_transitive), (1, 1));

    let strict = store.blast_radius_resolved("target", 200, true, 2).unwrap();
    assert!(strict.files.contains(&"/a/mid.rs".to_string()));
    assert!(
        !strict.files.contains(&"/c/far.rs".to_string()),
        "strict must drop bare-tier transitive callers"
    );
}

#[test]
fn blast_radius_scoped_resolution_filters_and_confirms_transitive_callers() {
    let mut store = Store::open_in_memory().unwrap();
    // Direct caller /r/src/mid.rs exports `helper`, which is also defined in
    // /q/src/other.rs. Three transitive candidates:
    //   /r/src/far/user.rs  imports super::super::mid → resolves to mid → CONFIRMED
    //   /q/src/local.rs     same dir as other.rs → resolves to other, NOT mid → dropped
    //   /z/noimp.rs         no structural link → bare → kept fuzzy, dropped strict
    store
        .upsert_edges(&[
            edge("/r/src/mid.rs", "calls", "target"),
            edge("/r/src/mid.rs", "defines", "helper"),
            edge("/q/src/other.rs", "defines", "helper"),
            edge("/r/src/far/user.rs", "calls", "helper"),
            edge("/r/src/far/user.rs", "imports", "super::super::mid"),
            edge("/q/src/local.rs", "calls", "helper"),
            edge("/z/noimp.rs", "calls", "helper"),
        ])
        .unwrap();

    let fuzzy = store
        .blast_radius_resolved("target", 200, false, 2)
        .unwrap();
    assert!(fuzzy.files.contains(&"/r/src/far/user.rs".to_string()));
    assert!(
        !fuzzy.files.contains(&"/q/src/local.rs".to_string()),
        "a call resolved to a different definer is cross-noise even in default mode"
    );
    assert!(fuzzy.files.contains(&"/z/noimp.rs".to_string()));
    assert_eq!((fuzzy.scoped_transitive, fuzzy.bare_transitive), (1, 1));

    let strict = store.blast_radius_resolved("target", 200, true, 2).unwrap();
    assert!(
        strict.files.contains(&"/r/src/far/user.rs".to_string()),
        "an import-confirmed transitive caller survives strict"
    );
    assert!(!strict.files.contains(&"/z/noimp.rs".to_string()));
}

#[test]
fn blast_radius_depth_controls_transitive_reach() {
    let mut store = Store::open_in_memory().unwrap();
    // A reachability chain (all same dir, so each hop resolves cleanly):
    //   a.rs calls target()  → direct
    //   a.rs exports expA ; b.rs calls expA   → hop 2
    //   b.rs exports expB ; c.rs calls expB   → hop 3
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "target"),
            edge("/p/a.rs", "defines", "expA"),
            edge("/p/b.rs", "calls", "expA"),
            edge("/p/b.rs", "defines", "expB"),
            edge("/p/c.rs", "calls", "expB"),
        ])
        .unwrap();

    // depth 1 = direct callers only.
    let d1 = store
        .blast_radius_resolved("target", 200, false, 1)
        .unwrap();
    assert_eq!(d1.files, vec!["/p/a.rs".to_string()]);
    assert_eq!(d1.scoped_transitive + d1.bare_transitive, 0);

    // depth 2 = direct + one transitive hop (reaches b.rs, not c.rs).
    let d2 = store
        .blast_radius_resolved("target", 200, false, 2)
        .unwrap();
    assert!(d2.files.contains(&"/p/a.rs".to_string()));
    assert!(d2.files.contains(&"/p/b.rs".to_string()));
    assert!(
        !d2.files.contains(&"/p/c.rs".to_string()),
        "c.rs is two hops out — excluded at depth 2"
    );

    // depth 3 = reaches c.rs through the chain.
    let d3 = store
        .blast_radius_resolved("target", 200, false, 3)
        .unwrap();
    assert!(
        d3.files.contains(&"/p/c.rs".to_string()),
        "depth 3 reaches the far end of the chain"
    );
}

#[test]
fn blast_radius_deep_terminates_on_cycle() {
    let mut store = Store::open_in_memory().unwrap();
    // A cycle through exported symbols: a.rs is the direct caller; a→b via expA, b→a via expB.
    // A deep walk must visit each file once (included = visited set) and terminate.
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "target"),
            edge("/p/a.rs", "defines", "expA"),
            edge("/p/b.rs", "calls", "expA"),
            edge("/p/b.rs", "defines", "expB"),
            edge("/p/a.rs", "calls", "expB"),
        ])
        .unwrap();
    // A high depth must not loop forever.
    let r = store
        .blast_radius_resolved("target", 200, false, 5)
        .unwrap();
    assert!(r.files.contains(&"/p/a.rs".to_string()));
    assert!(r.files.contains(&"/p/b.rs".to_string()));
    assert_eq!(
        r.files.len(),
        2,
        "each file visited once — no cycle re-entry"
    );
}

#[test]
fn defines_count_counts_distinct_definers() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/a.rs", "defines", "parse"),
            edge("/b.rs", "defines", "parse"),
            edge("/c.rs", "defines", "unique"),
        ])
        .unwrap();
    assert_eq!(store.defines_count("parse").unwrap(), 2);
    assert_eq!(store.defines_count("unique").unwrap(), 1);
    assert_eq!(store.defines_count("absent").unwrap(), 0);
}

#[test]
fn last_indexed_at_for_root_is_prefix_scoped() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_chunks(&[
            dummy_chunk("/proj/a.rs", 0, "fn a() {}"),
            dummy_chunk("/projector/b.rs", 0, "fn b() {}"),
        ])
        .unwrap();
    // Pin distinct timestamps so we can prove prefix scoping picks the right rows and
    // that "/proj" does NOT absorb the "/projector" sibling.
    store
        .db_connection()
        .execute_batch(
            "UPDATE chunks SET indexed_at = 1000 WHERE entry_path = '/proj/a.rs';
             UPDATE chunks SET indexed_at = 2000 WHERE entry_path = '/projector/b.rs';",
        )
        .unwrap();

    assert_eq!(store.last_indexed_at_for_root("/proj").unwrap(), Some(1000));
    assert_eq!(
        store.last_indexed_at_for_root("/projector").unwrap(),
        Some(2000)
    );
    // A root with nothing indexed under it → None (auto-reindex skips these).
    assert_eq!(store.last_indexed_at_for_root("/nope").unwrap(), None);
}

#[test]
fn find_related_files_merges_both_directions() {
    let mut store = Store::open_in_memory().unwrap();
    // app calls `run` (defined in lib) → lib is a dependency of app.
    // util calls `helper` (defined in app) → util is a dependent of app.
    store
        .upsert_edges(&[
            edge("/app.rs", "calls", "run"),
            edge("/lib.rs", "defines", "run"),
            edge("/app.rs", "defines", "helper"),
            edge("/util.rs", "calls", "helper"),
        ])
        .unwrap();
    let related = store.find_related_files("/app.rs", 10).unwrap();
    let paths: Vec<&str> = related.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/lib.rs"), "dependency direction");
    assert!(paths.contains(&"/util.rs"), "dependent direction");
    assert!(!paths.contains(&"/app.rs"), "self excluded");
}

#[test]
fn find_cycles_detects_an_scc() {
    let mut store = Store::open_in_memory().unwrap();
    // a→b→c→a cycle (each calls a uniquely-defined symbol of the next), plus standalone d.
    store
        .upsert_edges(&[
            edge("/a.rs", "calls", "bsym"),
            edge("/b.rs", "defines", "bsym"),
            edge("/b.rs", "calls", "csym"),
            edge("/c.rs", "defines", "csym"),
            edge("/c.rs", "calls", "asym"),
            edge("/a.rs", "defines", "asym"),
            edge("/d.rs", "defines", "dsym"),
        ])
        .unwrap();
    let cycles = store.find_cycles("/", 400).unwrap();
    assert_eq!(cycles.len(), 1, "exactly one cycle");
    assert_eq!(cycles[0], vec!["/a.rs", "/b.rs", "/c.rs"]);
    // No false cycle without a back-edge.
    let mut store2 = Store::open_in_memory().unwrap();
    store2
        .upsert_edges(&[
            edge("/x.rs", "calls", "ysym"),
            edge("/y.rs", "defines", "ysym"),
        ])
        .unwrap();
    assert!(store2.find_cycles("/", 400).unwrap().is_empty());
}
