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
    for e in &graph.edges {
        println!(
            "{:>3}  {} → {}",
            e.weight,
            basename(&e.from),
            basename(&e.to)
        );
    }
    println!();
    println!("(edge weight = number of shared call→define symbols)");
    Ok(())
}
