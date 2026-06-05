use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::require_index_db;

/// Resolve and expand a path, canonicalizing `~` prefixes.
fn expand(p: &str) -> String {
    shellexpand::tilde(p).into_owned()
}

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

pub(crate) async fn cmd_graph(path: String, limit: usize) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let scope = expand(&path);
    let graph = store.code_graph(&scope, limit)?;

    if graph.edges.is_empty() {
        println!("No call edges under \"{scope}\".");
        println!("Run `indexa deep {path}` on source files first (Rust/Python/JS/TS/Go/Java).");
        return Ok(());
    }

    println!(
        "Call graph under \"{scope}\": {} files, {} edges{}",
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
    for e in &graph.edges {
        println!(
            "{:>3}  {} → {}",
            e.weight,
            basename(&e.from),
            basename(&e.to)
        );
    }
    println!();
    println!(
        "(edge weight = number of shared call→define symbols; centrality = weighted PageRank)"
    );
    Ok(())
}
