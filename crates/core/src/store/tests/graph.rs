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
fn code_graph_drops_vendored_noise_edges() {
    let mut store = Store::open_in_memory().unwrap();
    // A vendored edge alongside a real one; the vendored endpoint must never appear.
    store
        .upsert_edges(&[
            edge("/p/app.rs", "calls", "real_sym"),
            edge("/p/lib.rs", "defines", "real_sym"),
            edge("/p/vendor/lib.rs", "calls", "noisy_sym"),
            edge("/p/vendor/other.rs", "defines", "noisy_sym"),
        ])
        .unwrap();
    let g = store.code_graph("/p", 400, false).unwrap();
    assert_eq!(g.edges.len(), 1);
    assert_eq!(g.edges[0].from, "/p/app.rs");
    assert_eq!(g.edges[0].to, "/p/lib.rs");
    assert!(
        g.nodes.iter().all(|n| !n.path.contains("/vendor/")),
        "no vendored node should survive either"
    );
}

/// B3: the filter must run BEFORE sort+truncate, not after — a test that only checks
/// "no vendor edges in the output" would pass even with the filter placed post-truncate
/// (it would just silently shrink the result below `max_edges`). This test discriminates
/// placement: the noisy pair outweighs both real pairs (2 shared symbols vs 1 each), so
/// with `max_edges` set to exactly the real-edge count, a post-truncate filter would let
/// the heavier noise edge win a slot and only ONE real edge would survive; a pre-truncate
/// filter drops the noise first and both real edges survive.
#[test]
fn code_graph_filters_noise_before_truncating_not_after() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            // Noise: weight 2 (two shared symbols) — sorts ahead of both real edges.
            edge("/p/vendor/lib.rs", "calls", "ns1"),
            edge("/p/vendor/lib.rs", "calls", "ns2"),
            edge("/p/vendor/other.rs", "defines", "ns1"),
            edge("/p/vendor/other.rs", "defines", "ns2"),
            // Two real edges, weight 1 each.
            edge("/p/real1.rs", "calls", "r1sym"),
            edge("/p/def1.rs", "defines", "r1sym"),
            edge("/p/real2.rs", "calls", "r2sym"),
            edge("/p/def2.rs", "defines", "r2sym"),
        ])
        .unwrap();
    // max_edges == exactly the real-edge count: a post-truncate filter would have
    // already dropped one real edge in favor of the heavier noise edge.
    let g = store.code_graph("/p", 2, false).unwrap();
    assert_eq!(
        g.edges.len(),
        2,
        "both real edges must survive — the noise edge never consumed a slot"
    );
    assert!(g
        .edges
        .iter()
        .any(|e| e.from == "/p/real1.rs" && e.to == "/p/def1.rs"));
    assert!(g
        .edges
        .iter()
        .any(|e| e.from == "/p/real2.rs" && e.to == "/p/def2.rs"));
    assert!(g
        .edges
        .iter()
        .all(|e| !e.from.contains("/vendor/") && !e.to.contains("/vendor/")));
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
        .blast_radius_resolved("target", 200, false, 2, false)
        .unwrap();
    assert!(fuzzy.files.contains(&"/a/mid.rs".to_string()));
    assert!(
        fuzzy.files.contains(&"/c/far.rs".to_string()),
        "default mode keeps the bare transitive hop (labeled)"
    );
    assert_eq!((fuzzy.direct, fuzzy.bare_transitive), (1, 1));

    let strict = store
        .blast_radius_resolved("target", 200, true, 2, false)
        .unwrap();
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
        .blast_radius_resolved("target", 200, false, 2, false)
        .unwrap();
    assert!(fuzzy.files.contains(&"/r/src/far/user.rs".to_string()));
    assert!(
        !fuzzy.files.contains(&"/q/src/local.rs".to_string()),
        "a call resolved to a different definer is cross-noise even in default mode"
    );
    assert!(fuzzy.files.contains(&"/z/noimp.rs".to_string()));
    assert_eq!((fuzzy.scoped_transitive, fuzzy.bare_transitive), (1, 1));

    let strict = store
        .blast_radius_resolved("target", 200, true, 2, false)
        .unwrap();
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
        .blast_radius_resolved("target", 200, false, 1, false)
        .unwrap();
    assert_eq!(d1.files, vec!["/p/a.rs".to_string()]);
    assert_eq!(d1.scoped_transitive + d1.bare_transitive, 0);

    // depth 2 = direct + one transitive hop (reaches b.rs, not c.rs).
    let d2 = store
        .blast_radius_resolved("target", 200, false, 2, false)
        .unwrap();
    assert!(d2.files.contains(&"/p/a.rs".to_string()));
    assert!(d2.files.contains(&"/p/b.rs".to_string()));
    assert!(
        !d2.files.contains(&"/p/c.rs".to_string()),
        "c.rs is two hops out — excluded at depth 2"
    );

    // depth 3 = reaches c.rs through the chain.
    let d3 = store
        .blast_radius_resolved("target", 200, false, 3, false)
        .unwrap();
    assert!(
        d3.files.contains(&"/p/c.rs".to_string()),
        "depth 3 reaches the far end of the chain"
    );

    // by_hop / grouped_by_hop (1.1): a.rs is hop 1 (direct), b.rs hop 2, c.rs hop 3 — each
    // file's first-inclusion hop, matching the WILL BREAK / LIKELY AFFECTED / MAY NEED
    // TESTING grouping.
    let hop_of = |r: &crate::store::BlastRadius, path: &str| {
        r.by_hop.iter().find(|(p, _)| p == path).map(|(_, h)| *h)
    };
    assert_eq!(hop_of(&d3, "/p/a.rs"), Some(1));
    assert_eq!(hop_of(&d3, "/p/b.rs"), Some(2));
    assert_eq!(hop_of(&d3, "/p/c.rs"), Some(3));

    let grouped = d3.grouped_by_hop();
    assert_eq!(grouped[0], (1, vec!["/p/a.rs".to_string()]));
    assert_eq!(grouped[1], (2, vec!["/p/b.rs".to_string()]));
    assert_eq!(grouped[2], (3, vec!["/p/c.rs".to_string()]));

    // risk(): 1 direct caller → LOW.
    assert_eq!(d3.risk(), crate::store::BlastRadiusRisk::Low);
}

#[test]
fn blast_radius_risk_scales_with_direct_caller_count() {
    let mut store = Store::open_in_memory().unwrap();
    // 12 distinct direct callers of "target" → HIGH (>=10).
    let edges: Vec<_> = (0..12)
        .map(|i| edge(&format!("/p/caller{i}.rs"), "calls", "target"))
        .collect();
    store.upsert_edges(&edges).unwrap();
    let r = store
        .blast_radius_resolved("target", 200, false, 1, false)
        .unwrap();
    assert_eq!(r.direct, 12);
    assert_eq!(r.risk(), crate::store::BlastRadiusRisk::High);
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
        .blast_radius_resolved("target", 200, false, 5, false)
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

#[test]
fn heritage_edges_migration_widens_old_check_constraint() {
    // 2.2: a DB previously migrated only as far as the 'calls'-widened CHECK (imports/
    // defines/calls, no extends/implements) must widen further on the next open, without
    // losing existing rows. Simulates that prior state directly, then re-runs init_schema
    // (what a real Store::open on an old DB triggers).
    let mut store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute_batch(
            "DROP TABLE edges;
             CREATE TABLE edges (
                 from_path TEXT NOT NULL,
                 kind      TEXT NOT NULL CHECK(kind IN ('imports','defines','calls')),
                 to_ref    TEXT NOT NULL,
                 PRIMARY KEY (from_path, kind, to_ref)
             ) WITHOUT ROWID;
             INSERT INTO edges VALUES ('/a.rs', 'calls', 'foo');
             PRAGMA user_version = 1;",
        )
        .unwrap();

    store.init_schema().unwrap();

    // The pre-existing row survived the copy-table migration.
    let preserved: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE from_path='/a.rs' AND kind='calls' AND to_ref='foo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        preserved, 1,
        "existing 'calls' row must survive the migration"
    );

    // The widened CHECK now accepts 'extends'/'implements'.
    store
        .conn
        .execute("INSERT INTO edges VALUES ('/a.rs', 'extends', 'Base')", [])
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO edges VALUES ('/a.rs', 'implements', 'Trait')",
            [],
        )
        .unwrap();
}

// The four tests below lock the fix for a silent-row-loss bug in the two edges CHECK-widening
// migrations (the 'calls' widening just above `edges_allows_calls`'s gate, and the heritage
// widening covered by `heritage_edges_migration_widens_old_check_constraint` above): both used
// `INSERT OR IGNORE INTO edges_new SELECT * FROM edges`, which silently drops any row that fails
// to insert instead of failing the migration loudly. Both copies are now an explicit-column plain
// `INSERT`. Row-preservation tests alone don't discriminate the fix from the bug — a normal valid
// row inserts identically either way, since the CHECK is only ever *widened*, never narrowed, so
// nothing a legitimate prior schema allowed can violate the new one. The "…fails_loudly…" tests
// below construct a row that violates even the OLD CHECK (via `PRAGMA ignore_check_constraints`,
// simulating already-corrupt data) to prove the two behaviors actually differ: verified by hand
// that reverting schema.rs to `INSERT OR IGNORE` turns both red (`Store::open` then succeeds and
// silently drops the offending row instead of erroring).

#[test]
fn migrates_legacy_edges_check_preserving_all_rows() {
    // Pre-'calls' indexes had `edges.kind CHECK IN ('imports','defines')`. Store::open must widen
    // the CHECK to include 'calls' AND preserve every legacy row — the table-recreate must not
    // silently drop rows (the reason the copy is a plain explicit INSERT, not INSERT OR IGNORE).
    // Note: starting from the 2-value CHECK with `user_version` at its default (0) runs BOTH
    // migrations in one `init_schema` call (2-value → 3-value here, then 3-value → 5-value via the
    // heritage migration, since its gate is also unmet) — this test exercises the 'calls' copy
    // specifically via the row content (no 'extends'/'implements' rows involved), while
    // `migrates_legacy_edges_heritage_check_preserving_all_rows` below isolates the heritage copy
    // alone by starting from the already-3-value CHECK.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pre_calls.db");
    {
        // Minimal legacy edges table with the OLD 2-value CHECK + seeded imports/defines rows.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE edges (
                 from_path TEXT NOT NULL,
                 kind      TEXT NOT NULL CHECK(kind IN ('imports','defines')),
                 to_ref    TEXT NOT NULL,
                 PRIMARY KEY (from_path, kind, to_ref)
             ) WITHOUT ROWID;
             INSERT INTO edges (from_path, kind, to_ref) VALUES
                 ('/a.rs','imports','std::fs'),
                 ('/a.rs','defines','parse'),
                 ('/b.rs','defines','run'),
                 ('/b.rs','imports','/a.rs');",
        )
        .unwrap();
    }

    // Store::open runs the CHECK-widening migration(s).
    let mut store = Store::open(&path).expect("must open & migrate a pre-'calls' index");

    // 1) Row parity: all four legacy rows survive.
    let mut got: Vec<(String, String, String)> = store
        .all_edges()
        .unwrap()
        .into_iter()
        .map(|e| (e.from_path, e.kind, e.to_ref))
        .collect();
    got.sort();
    let mut want: Vec<(String, String, String)> = vec![
        ("/a.rs", "defines", "parse"),
        ("/a.rs", "imports", "std::fs"),
        ("/b.rs", "defines", "run"),
        ("/b.rs", "imports", "/a.rs"),
    ]
    .into_iter()
    .map(|(f, k, t)| (f.to_string(), k.to_string(), t.to_string()))
    .collect();
    want.sort();
    assert_eq!(
        got, want,
        "every legacy edge must survive the CHECK-widening migration"
    );

    // 2) The CHECK is now wide enough for 'calls' edges (the point of the migration).
    store
        .upsert_edges(&[edge("/c.rs", "calls", "parse")])
        .expect("'calls' edges must be accepted after migration");
    assert_eq!(store.all_edges().unwrap().len(), 5);
}

#[test]
fn migrates_legacy_edges_check_fails_loudly_on_a_corrupt_row() {
    // A row that violates even the OLD 2-value CHECK (only constructable by bypassing SQLite's
    // own enforcement, here via `PRAGMA ignore_check_constraints` — simulating data corruption
    // that predates this fix) must fail the migration loudly, not vanish silently. This is the
    // test that actually discriminates the fix from `INSERT OR IGNORE`: a merely-valid row
    // inserts identically under both, so `migrates_legacy_edges_check_preserving_all_rows` alone
    // would stay green even with the bug still in place.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt_pre_calls.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE edges (
                 from_path TEXT NOT NULL,
                 kind      TEXT NOT NULL CHECK(kind IN ('imports','defines')),
                 to_ref    TEXT NOT NULL,
                 PRIMARY KEY (from_path, kind, to_ref)
             ) WITHOUT ROWID;
             INSERT INTO edges (from_path, kind, to_ref) VALUES ('/a.rs','imports','std::fs');
             PRAGMA ignore_check_constraints = ON;
             INSERT INTO edges (from_path, kind, to_ref) VALUES ('/x.rs','bogus','y');
             PRAGMA ignore_check_constraints = OFF;",
        )
        .unwrap();
    }

    // Store::open must surface an error, not silently open a DB missing the corrupt row.
    assert!(
        Store::open(&path).is_err(),
        "a row the new CHECK rejects must fail the migration, not vanish silently"
    );

    // The IMMEDIATE transaction rolled back on failure: the original table (both rows, including
    // the corrupt one) is untouched — nothing was lost, nothing was partially applied.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "a failed migration must not lose or partially apply rows"
    );
}

#[test]
fn migrates_legacy_edges_heritage_check_preserving_all_rows() {
    // Pre-heritage indexes had the 'calls'-widened `edges.kind CHECK IN ('imports','defines',
    // 'calls')` (post site-1 migration, pre-2.2). Store::open must widen the CHECK further to
    // include 'extends'/'implements' AND preserve every legacy row. Starting from the 3-value
    // CHECK means `edges_allows_calls` is already true, so this isolates the heritage copy alone
    // (the 'calls' copy above is skipped) — the companion test to
    // `migrates_legacy_edges_check_preserving_all_rows` for the second migration site.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pre_heritage.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE edges (
                 from_path TEXT NOT NULL,
                 kind      TEXT NOT NULL CHECK(kind IN ('imports','defines','calls')),
                 to_ref    TEXT NOT NULL,
                 PRIMARY KEY (from_path, kind, to_ref)
             ) WITHOUT ROWID;
             INSERT INTO edges (from_path, kind, to_ref) VALUES
                 ('/a.rs','imports','std::fs'),
                 ('/a.rs','defines','parse'),
                 ('/b.rs','defines','run'),
                 ('/b.rs','calls','parse');",
        )
        .unwrap();
    }

    let mut store = Store::open(&path).expect("must open & migrate a pre-heritage index");

    // 1) Row parity: all four legacy rows survive.
    let mut got: Vec<(String, String, String)> = store
        .all_edges()
        .unwrap()
        .into_iter()
        .map(|e| (e.from_path, e.kind, e.to_ref))
        .collect();
    got.sort();
    let mut want: Vec<(String, String, String)> = vec![
        ("/a.rs", "defines", "parse"),
        ("/a.rs", "imports", "std::fs"),
        ("/b.rs", "calls", "parse"),
        ("/b.rs", "defines", "run"),
    ]
    .into_iter()
    .map(|(f, k, t)| (f.to_string(), k.to_string(), t.to_string()))
    .collect();
    want.sort();
    assert_eq!(
        got, want,
        "every pre-heritage edge must survive the CHECK-widening migration"
    );

    // 2) The CHECK is now wide enough for 'extends'/'implements' (the point of the migration).
    store
        .upsert_edges(&[
            edge("/c.rs", "extends", "Base"),
            edge("/c.rs", "implements", "Trait"),
        ])
        .expect("heritage edges must be accepted after migration");
    assert_eq!(store.all_edges().unwrap().len(), 6);
}

#[test]
fn migrates_legacy_edges_heritage_check_fails_loudly_on_a_corrupt_row() {
    // Companion to `migrates_legacy_edges_check_fails_loudly_on_a_corrupt_row` for the second
    // migration site: a row that violates even the pre-heritage (3-value) CHECK must fail the
    // heritage-widening migration loudly rather than vanish silently.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt_pre_heritage.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE edges (
                 from_path TEXT NOT NULL,
                 kind      TEXT NOT NULL CHECK(kind IN ('imports','defines','calls')),
                 to_ref    TEXT NOT NULL,
                 PRIMARY KEY (from_path, kind, to_ref)
             ) WITHOUT ROWID;
             INSERT INTO edges (from_path, kind, to_ref) VALUES ('/a.rs','calls','parse');
             PRAGMA ignore_check_constraints = ON;
             INSERT INTO edges (from_path, kind, to_ref) VALUES ('/x.rs','bogus','y');
             PRAGMA ignore_check_constraints = OFF;",
        )
        .unwrap();
    }

    assert!(
        Store::open(&path).is_err(),
        "a row the widened CHECK rejects must fail the migration, not vanish silently"
    );

    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "a failed migration must not lose or partially apply rows"
    );
}

#[test]
fn changed_impact_merges_blast_radii_across_symbols_keeping_the_smallest_hop() {
    let mut store = Store::open_in_memory().unwrap();
    // fileX is a direct (hop 1) caller of funcA, but only a transitive (hop 2) caller of
    // funcB (via callerB's export) — the merge must keep the smaller of the two hops.
    store
        .upsert_edges(&[
            edge("/p/callerB.rs", "calls", "funcB"),
            edge("/p/callerB.rs", "defines", "expB"),
            edge("/p/fileX.rs", "calls", "expB"),
            edge("/p/fileX.rs", "calls", "funcA"),
        ])
        .unwrap();

    let merged = store
        .changed_impact(
            &["funcA".to_string(), "funcB".to_string()],
            200,
            false,
            2,
            false,
        )
        .unwrap();
    let hop_of_filex = merged
        .by_hop
        .iter()
        .find(|(p, _)| p == "/p/fileX.rs")
        .map(|(_, h)| *h);
    assert_eq!(
        hop_of_filex,
        Some(1),
        "fileX is hop 1 via funcA even though it's hop 2 via funcB — min wins"
    );
    assert!(merged.files.contains(&"/p/callerB.rs".to_string()));
    assert_eq!(
        merged.direct, 2,
        "one direct caller of funcA + one of funcB"
    );
}

#[test]
fn changed_impact_dedupes_repeated_names_and_empty_input_is_empty() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[edge("/p/a.rs", "calls", "target")])
        .unwrap();
    let once = store
        .changed_impact(&["target".to_string()], 200, false, 2, false)
        .unwrap();
    let dup = store
        .changed_impact(
            &["target".to_string(), "target".to_string()],
            200,
            false,
            2,
            false,
        )
        .unwrap();
    assert_eq!(
        once.direct, dup.direct,
        "a repeated symbol name must not double-count"
    );

    let empty = store.changed_impact(&[], 200, false, 2, false).unwrap();
    assert!(empty.files.is_empty());
}

#[test]
fn trace_path_finds_the_shortest_chain_through_a_scoped_call() {
    let mut store = Store::open_in_memory().unwrap();
    // handler.rs calls helper() (same-dir def in mid.rs); mid.rs imports and calls
    // db_query() defined in db.rs. Path: handler.rs -> mid.rs -> db.rs.
    store
        .upsert_edges(&[
            edge("/p/handler.rs", "calls", "helper"),
            edge("/p/mid.rs", "defines", "helper"),
            edge("/p/mid.rs", "calls", "db_query"),
            edge("/p/mid.rs", "imports", "db"),
            edge("/p/db.rs", "defines", "db_query"),
        ])
        .unwrap();

    let path = store
        .trace_path("/p/handler.rs", "/p/db.rs", 5)
        .unwrap()
        .expect("a path should be found");
    let files: Vec<&str> = path.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(files, vec!["/p/handler.rs", "/p/mid.rs", "/p/db.rs"]);
    // First hop is the start placeholder; the rest carry the resolving tier.
    assert_eq!(path[0].tier, ResolutionTier::SameFile);
    assert_eq!(path[1].tier, ResolutionTier::SameDir);
}

#[test]
fn trace_path_from_and_to_accept_bare_symbol_names() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/p/a.rs", "defines", "start_sym"),
            edge("/p/a.rs", "calls", "target_sym"),
            edge("/p/b.rs", "defines", "target_sym"),
        ])
        .unwrap();

    let path = store
        .trace_path("start_sym", "target_sym", 5)
        .unwrap()
        .expect("a path should be found via bare symbol resolution");
    let files: Vec<&str> = path.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(files, vec!["/p/a.rs", "/p/b.rs"]);
}

#[test]
fn trace_path_returns_none_when_unreachable_or_depth_exceeded() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/p/isolated.rs", "defines", "lonely"),
            edge("/p/other.rs", "defines", "elsewhere"),
            // A long chain the depth cap must cut off.
            edge("/p/a.rs", "calls", "b_sym"),
            edge("/p/b.rs", "defines", "b_sym"),
            edge("/p/b.rs", "calls", "c_sym"),
            edge("/p/c.rs", "defines", "c_sym"),
        ])
        .unwrap();

    assert!(store
        .trace_path("/p/isolated.rs", "/p/other.rs", 5)
        .unwrap()
        .is_none());
    // Reachable at depth 2 (a.rs -> b.rs -> c.rs) but not within depth 1.
    assert!(store.trace_path("/p/a.rs", "/p/c.rs", 1).unwrap().is_none());
    assert!(store.trace_path("/p/a.rs", "/p/c.rs", 2).unwrap().is_some());
}

#[test]
fn trace_path_same_start_and_target_is_a_trivial_one_node_path() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[edge("/p/a.rs", "calls", "whatever")])
        .unwrap();
    let path = store.trace_path("/p/a.rs", "/p/a.rs", 5).unwrap().unwrap();
    assert_eq!(path.len(), 1);
    assert_eq!(path[0].path, "/p/a.rs");
}

// ── Dependency closure — open-ended transitive walk, complementary to trace_path ──────

#[test]
fn dependency_closure_callee_direction_depth_controls_reach() {
    let mut store = Store::open_in_memory().unwrap();
    // A callee chain (same dir throughout, so every hop resolves same-dir cleanly):
    //   a.rs calls helper()                        → direct
    //   helper defined in b.rs; b.rs calls inner()  → hop 2
    //   inner defined in c.rs; c.rs calls leaf()    → hop 3
    //   leaf defined in d.rs
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "helper"),
            edge("/p/b.rs", "defines", "helper"),
            edge("/p/b.rs", "calls", "inner"),
            edge("/p/c.rs", "defines", "inner"),
            edge("/p/c.rs", "calls", "leaf"),
            edge("/p/d.rs", "defines", "leaf"),
        ])
        .unwrap();

    let d1 = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 1, false, 200)
        .unwrap();
    assert_eq!(d1.seeds, vec!["/p/a.rs".to_string()]);
    assert_eq!(d1.files, vec!["/p/b.rs".to_string()]);
    assert_eq!(d1.total, 1);
    assert_eq!(d1.scoped, 1);
    assert_eq!(d1.bare, 0);

    let d2 = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 2, false, 200)
        .unwrap();
    assert_eq!(d2.files, vec!["/p/b.rs".to_string(), "/p/c.rs".to_string()]);
    assert!(
        !d2.files.contains(&"/p/d.rs".to_string()),
        "d.rs is three hops out — excluded at depth 2"
    );

    let d3 = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 3, false, 200)
        .unwrap();
    assert!(
        d3.files.contains(&"/p/d.rs".to_string()),
        "depth 3 reaches the far end of the chain"
    );
    assert_eq!(d3.total, 3);
}

#[test]
fn dependency_closure_caller_direction_mirrors_the_callee_walk() {
    let mut store = Store::open_in_memory().unwrap();
    // Same chain as the callee test above — walked backward from the far end (d.rs).
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "helper"),
            edge("/p/b.rs", "defines", "helper"),
            edge("/p/b.rs", "calls", "inner"),
            edge("/p/c.rs", "defines", "inner"),
            edge("/p/c.rs", "calls", "leaf"),
            edge("/p/d.rs", "defines", "leaf"),
        ])
        .unwrap();

    let d1 = store
        .dependency_closure("/p/d.rs", ClosureDirection::Caller, 1, false, 200)
        .unwrap();
    assert_eq!(d1.files, vec!["/p/c.rs".to_string()]);

    let d2 = store
        .dependency_closure("/p/d.rs", ClosureDirection::Caller, 2, false, 200)
        .unwrap();
    assert_eq!(d2.files, vec!["/p/b.rs".to_string(), "/p/c.rs".to_string()]);

    let d3 = store
        .dependency_closure("/p/d.rs", ClosureDirection::Caller, 3, false, 200)
        .unwrap();
    assert_eq!(
        d3.files,
        vec![
            "/p/a.rs".to_string(),
            "/p/b.rs".to_string(),
            "/p/c.rs".to_string()
        ]
    );
}

#[test]
fn dependency_closure_accepts_a_bare_symbol_seed_in_either_direction() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/p/entry.rs", "defines", "entry_fn"),
            edge("/p/entry.rs", "calls", "helper"),
            edge("/p/helper.rs", "defines", "helper"),
        ])
        .unwrap();

    // "entry_fn" is not a `from_path` in `edges` — it's a bare symbol, so the closure
    // must resolve it to its definer file (/p/entry.rs) and start the callee walk there.
    let callee = store
        .dependency_closure("entry_fn", ClosureDirection::Callee, 1, false, 200)
        .unwrap();
    assert_eq!(callee.seeds, vec!["/p/entry.rs".to_string()]);
    assert_eq!(callee.files, vec!["/p/helper.rs".to_string()]);

    // "helper" likewise resolves to /p/helper.rs and the caller walk finds entry.rs.
    let caller = store
        .dependency_closure("helper", ClosureDirection::Caller, 1, false, 200)
        .unwrap();
    assert_eq!(caller.seeds, vec!["/p/helper.rs".to_string()]);
    assert_eq!(caller.files, vec!["/p/entry.rs".to_string()]);
}

#[test]
fn dependency_closure_excludes_seed_files_and_terminates_on_a_cycle_in_either_direction() {
    let mut store = Store::open_in_memory().unwrap();
    // Cycle: a.rs calls B (defined in b.rs); b.rs calls A (defined in a.rs).
    store
        .upsert_edges(&[
            edge("/p/a.rs", "defines", "a_fn"),
            edge("/p/a.rs", "calls", "b_fn"),
            edge("/p/b.rs", "defines", "b_fn"),
            edge("/p/b.rs", "calls", "a_fn"),
        ])
        .unwrap();

    let callee = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 5, false, 200)
        .unwrap();
    // b.rs is a real dependency; a.rs (the seed) must never appear in its own closure,
    // even though the cycle loops back to it — and a high depth must not hang.
    assert_eq!(callee.files, vec!["/p/b.rs".to_string()]);

    let caller = store
        .dependency_closure("/p/a.rs", ClosureDirection::Caller, 5, false, 200)
        .unwrap();
    assert_eq!(caller.files, vec!["/p/b.rs".to_string()]);
}

#[test]
fn dependency_closure_on_unknown_target_returns_empty() {
    let store = Store::open_in_memory().unwrap();
    let closure = store
        .dependency_closure("/nowhere.rs", ClosureDirection::Callee, 2, false, 200)
        .unwrap();
    assert!(closure.files.is_empty());
    assert!(closure.seeds.is_empty());
    assert_eq!(closure.total, 0);
}

#[test]
fn dependency_closure_drops_noise_targets_without_inflating_tier_counters() {
    let mut store = Store::open_in_memory().unwrap();
    // a.rs's only call resolves to a vendored file — the noise filter must drop it
    // from `files` AND must not leave `scoped`/`bare` claiming a resolution that
    // produced zero visible files.
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "vend"),
            edge("/p/node_modules/v.js", "defines", "vend"),
        ])
        .unwrap();

    let closure = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 2, false, 200)
        .unwrap();
    assert!(closure.files.is_empty(), "vendored target must be dropped");
    assert_eq!(closure.total, 0);
    assert_eq!(
        closure.scoped, 0,
        "a resolution with no surviving files must not count as scoped"
    );
    assert_eq!(
        closure.bare, 0,
        "a resolution with no surviving files must not count as bare either"
    );
}

#[test]
fn dependency_closure_strict_drops_the_bare_tier_entirely() {
    let mut store = Store::open_in_memory().unwrap();
    // Different directories, no import edges recorded → resolves to the bare tier
    // (no same-dir/import evidence linking the call to its definer).
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "widget"),
            edge("/q/other.rs", "defines", "widget"),
        ])
        .unwrap();

    let loose = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 1, false, 200)
        .unwrap();
    assert_eq!(loose.files, vec!["/q/other.rs".to_string()]);
    assert_eq!(loose.bare, 1);
    assert_eq!(loose.scoped, 0);

    let strict = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 1, true, 200)
        .unwrap();
    assert!(
        strict.files.is_empty(),
        "strict must drop the bare-tier result"
    );
    assert_eq!(strict.bare, 0);
    assert_eq!(strict.scoped, 0);
}

#[test]
fn dependency_closure_caps_files_but_reports_the_true_total() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_edges(&[
            edge("/p/a.rs", "calls", "x"),
            edge("/p/x.rs", "defines", "x"),
            edge("/p/a.rs", "calls", "y"),
            edge("/p/y.rs", "defines", "y"),
            edge("/p/a.rs", "calls", "z"),
            edge("/p/z.rs", "defines", "z"),
        ])
        .unwrap();

    let closure = store
        .dependency_closure("/p/a.rs", ClosureDirection::Callee, 1, false, 2)
        .unwrap();
    assert_eq!(closure.total, 3, "3 files are reachable before the cap");
    assert_eq!(closure.files.len(), 2, "output is capped at `limit`");
    assert_eq!(
        closure.files,
        vec!["/p/x.rs".to_string(), "/p/y.rs".to_string()],
        "capped output keeps the first `limit` in sorted order"
    );
    assert_eq!(
        closure.scoped, 3,
        "the true resolved-edge count is unaffected by the cap"
    );
}
