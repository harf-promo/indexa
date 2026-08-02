use anyhow::Result;
use indexa_core::config::Config;
use indexa_core::store::{BlastRadius, ResolutionTier, Store};

use super::helpers::{build_llm, expand, require_index_db};

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// Render a path relative to the queried scope (e.g. `crates/embed/src/ollama.rs`)
/// so same-named files in the "most central" list stay distinguishable — a
/// basename alone is ambiguous, and in a `<crate>/src/<file>` layout so are the
/// last two components.
fn rel_to_scope(path: &str, scope: &str) -> String {
    let base = scope.trim_end_matches('/');
    path.strip_prefix(base)
        .map(|r| r.trim_start_matches('/'))
        .filter(|r| !r.is_empty())
        .unwrap_or(path)
        .to_owned()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_graph(
    cfg: &Config,
    path: String,
    limit: usize,
    strict: bool,
    cycles: bool,
    blast: Option<String>,
    depth: usize,
    grouped: bool,
    heritage: bool,
    compute_co_change: bool,
    compute_modules: bool,
    modules: bool,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let scope = expand(&path);

    // --compute-co-change: recompute the co_change table from git history and return —
    // every other flag is ignored in this mode (2.7).
    if compute_co_change {
        let mut store = Store::open(&db_path)?;
        let root = std::path::Path::new(&scope);
        let pairs = indexa_core::cochange::co_change_pairs(
            root,
            indexa_core::cochange::DEFAULT_COMMIT_LIMIT,
        )?;
        if pairs.is_empty() {
            println!(
                "No co-change pairs found under \"{scope}\" (not a git repo, no history, or every commit touched only one file)."
            );
            return Ok(());
        }
        let pair_count = pairs.len();
        store.replace_co_change(&pairs)?;
        println!("Computed {pair_count} co-change pair(s) under \"{scope}\" and stored them.");
        println!("Run `indexa related --include-co-change <file>` to see them.");
        return Ok(());
    }

    // --compute-modules: recompute the persisted architecture map (4.6) and return — every
    // other flag is ignored in this mode, matching --compute-co-change's precedent.
    if compute_modules {
        let mut store = Store::open(&db_path)?;
        let llm = build_llm(cfg, Some(&cfg.describer.dir_model))?;
        let count =
            indexa_query::modules::recompute_graph_modules(&mut store, llm.as_ref(), &scope, 5000)
                .await?;
        if count == 0 {
            println!("No call graph under \"{scope}\" to cluster — nothing computed.");
            println!("Run `indexa deep {path}` on source files first.");
            return Ok(());
        }
        println!("Computed {count} architecture-map module(s) under \"{scope}\" and stored them.");
        println!("Run `indexa graph --modules {path}` to see them.");
        return Ok(());
    }

    // --modules: show the persisted architecture map instead of the whole-scope graph.
    if modules {
        let store = Store::open(&db_path)?;
        let found = store.graph_modules_for_scope(&scope)?;
        if found.is_empty() {
            println!("No architecture-map modules under \"{scope}\".");
            println!("Run `indexa graph --compute-modules {path}` first.");
            return Ok(());
        }
        println!("{} module(s) under \"{scope}\":", found.len());
        println!("{}", "─".repeat(60));
        for m in &found {
            println!(
                "\n📦 {} (cohesion {:.2}, {} file(s)):",
                m.label,
                m.cohesion,
                m.members.len()
            );
            for p in &m.members {
                println!("  {}", rel_to_scope(p, &scope));
            }
        }
        return Ok(());
    }

    let store = Store::open(&db_path)?;

    // --blast <symbol>: "what breaks if I change this?" — the caller reachability set to
    // `depth` hops, instead of the whole-scope graph. `path` is ignored in this mode.
    if let Some(symbol) = blast {
        let depth = depth.clamp(1, 5);
        let radius =
            store.blast_radius_resolved(&symbol, limit.max(200), strict, depth, heritage)?;
        if radius.files.is_empty() {
            println!("No blast radius found for \"{symbol}\".");
            println!(
                "Run `indexa deep <path>` on source files first (Rust/Python/JS/TS/Go/Java/C/C++)."
            );
            return Ok(());
        }
        println!(
            "Blast radius of \"{symbol}\" (depth {depth}): {} file(s)",
            radius.files.len()
        );
        println!("{}", "─".repeat(60));
        if grouped {
            print_blast_radius_grouped(&radius);
        } else {
            for f in &radius.files {
                println!("  {}", basename(f));
            }
        }
        println!();
        println!(
            "direct callers: {} · transitive: {} resolution-confirmed + {} bare-name{}",
            radius.direct,
            radius.scoped_transitive,
            radius.bare_transitive,
            if strict {
                " (strict: bare fallback off)"
            } else {
                ""
            }
        );
        if radius.bare_transitive > 0 {
            println!(
                "({} transitive file(s) are approximate: {})",
                radius.bare_transitive,
                indexa_core::store::BARE_NAME_CAVEAT
            );
        }
        return Ok(());
    }

    // --cycles: report dependency cycles (Tarjan SCC over the call graph) and return.
    if cycles {
        let found = store.find_cycles(&scope, limit.max(500))?;
        if found.is_empty() {
            println!("No dependency cycles found under \"{scope}\". ✓");
            return Ok(());
        }
        println!(
            "Found {} dependency cycle(s) under \"{scope}\" (heuristic call resolution — verify):",
            found.len()
        );
        for (i, cycle) in found.iter().enumerate() {
            println!("\n  Cycle {} ({} files):", i + 1, cycle.len());
            for p in cycle {
                println!("    {}", basename(p));
            }
        }
        return Ok(());
    }

    let scoped = store.code_graph_scoped(&scope, limit, strict)?;
    let graph = &scoped.graph;

    if graph.edges.is_empty() {
        println!("No call edges under \"{scope}\".");
        if strict {
            println!(
                "(strict mode — only scope-resolved edges (same-dir/import). Try without --strict.)"
            );
        }
        println!(
            "Run `indexa deep {path}` on source files first (Rust/Python/JS/TS/Go/Java/C/C++)."
        );
        return Ok(());
    }

    println!(
        "Call graph under \"{scope}\" ({} mode): {} files, {} edges{}",
        if strict { "strict" } else { "scoped" },
        graph.nodes.len(),
        graph.edges.len(),
        if graph.truncated {
            " (truncated — heaviest shown)"
        } else {
            ""
        }
    );
    println!("{}", "─".repeat(60));

    // Most-central files by weighted PageRank, scored 0–100 relative to the top
    // hub — the files most worth reading first to understand the codebase.
    let max_pr = graph
        .nodes
        .iter()
        .map(|n| n.pagerank)
        .fold(0.0_f64, f64::max);
    let mut ranked: Vec<_> = graph.nodes.iter().collect();
    ranked.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("Most central files (centrality 0–100):");
    for n in ranked.iter().take(10) {
        let score = if max_pr > 0.0 {
            (n.pagerank / max_pr * 100.0).round() as i64
        } else {
            0
        };
        println!("{score:>3}  {}", rel_to_scope(&n.path, &scope));
    }
    println!();

    println!("Heaviest call edges:");
    for (e, tier) in graph.edges.iter().zip(&scoped.edge_tiers) {
        println!(
            "{:>3}  {} → {}{}",
            e.weight,
            basename(&e.from),
            basename(&e.to),
            // Only the bare remainder is approximate — flag it inline.
            if *tier == ResolutionTier::Bare {
                "  (bare)"
            } else {
                ""
            }
        );
    }
    println!();

    // Resolution-tier summary; the bare-name caveat applies ONLY to the bare remainder.
    let count = |t: ResolutionTier| scoped.edge_tiers.iter().filter(|x| **x == t).count();
    let (same_dir, import, bare) = (
        count(ResolutionTier::SameDir),
        count(ResolutionTier::Import),
        count(ResolutionTier::Bare),
    );
    println!(
        "edges: {} scoped ({same_dir} same-dir, {import} import-resolved) + {bare} bare-name",
        same_dir + import
    );
    println!(
        "(edge weight = number of shared call→define symbols; centrality = weighted PageRank)"
    );
    if bare > 0 {
        println!(
            "({bare} bare-name edge(s) are approximate: {} — see docs/methodology.md)",
            indexa_core::store::BARE_NAME_CAVEAT
        );
    } else {
        println!(
            "(no bare-name matches in this view; same-dir edges are proximity-matched, \
same-file/import are structural)"
        );
    }
    Ok(())
}

/// Hop → risk label, matching GitNexus's `impact` contract: hop 1 = direct callers (will
/// break immediately), hop 2 = one transitive step (likely affected), hop 3+ = further steps
/// (worth testing but less certain to break).
fn hop_risk_label(hop: usize) -> &'static str {
    match hop {
        1 => "WILL BREAK",
        2 => "LIKELY AFFECTED",
        _ => "MAY NEED TESTING",
    }
}

/// Print a blast radius grouped by hop with a risk label per group, plus an overall
/// LOW/MEDIUM/HIGH summary line — the `--grouped` rendering shared in spirit with the MCP
/// `blast_radius` tool's `grouped: true` output (same [`BlastRadius::grouped_by_hop`] data).
fn print_blast_radius_grouped(radius: &BlastRadius) {
    println!(
        "risk: {} ({} direct caller(s))",
        radius.risk().as_str(),
        radius.direct
    );
    for (hop, files) in radius.grouped_by_hop() {
        println!(
            "\n  hop {hop} — {} ({} file(s)):",
            hop_risk_label(hop),
            files.len()
        );
        for f in &files {
            println!("    {}", basename(f));
        }
    }
}
