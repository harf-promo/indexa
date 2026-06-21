use anyhow::Result;
use indexa_core::store::{ResolutionTier, Store};

use super::helpers::{expand, require_index_db};

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

pub(crate) async fn cmd_graph(
    path: String,
    limit: usize,
    strict: bool,
    cycles: bool,
    blast: Option<String>,
    depth: usize,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let scope = expand(&path);

    // --blast <symbol>: "what breaks if I change this?" — the caller reachability set to
    // `depth` hops, instead of the whole-scope graph. `path` is ignored in this mode.
    if let Some(symbol) = blast {
        let depth = depth.clamp(1, 5);
        let radius = store.blast_radius_resolved(&symbol, limit.max(200), strict, depth)?;
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
        for f in &radius.files {
            println!("  {}", basename(f));
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
