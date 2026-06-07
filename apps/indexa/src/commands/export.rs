use anyhow::{Context, Result};
use indexa_core::store::Store;
use indexa_query::{
    build_tree, render_graph, render_json, render_markdown, render_weights, render_xml,
};

use super::helpers::require_index_db;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_export(
    paths: Vec<String>,
    format: String,
    depth: Option<usize>,
    output: Option<String>,
    include_weights: bool,
    include_graph: bool,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let count = store.summary_count()?;
    if count == 0 {
        println!("No summaries found. Run `indexa summarize <path>` first.");
        return Ok(());
    }

    let roots: Vec<String> = if paths.is_empty() {
        // Export the roots of the summary tree (depth = 0).
        store
            .tree_level("")
            .unwrap_or_default()
            .into_iter()
            .map(|n| n.path)
            .collect()
    } else {
        paths
            .into_iter()
            .map(|p| shellexpand::tilde(&p).into_owned())
            .collect()
    };

    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_owned())
    };

    let mut out_buf = String::new();
    for root_path in &roots {
        let tree = build_tree(&store, root_path, depth)?;
        let Some(tree) = tree else {
            eprintln!(
                "No summary found for {root_path} — run `indexa summarize {root_path}` first."
            );
            continue;
        };
        let rendered = match format.as_str() {
            "md" | "markdown" => render_markdown(&tree),
            "json" => render_json(&tree),
            _ => render_xml(&tree, &now), // xml is the default
        };
        out_buf.push_str(&rendered);
        out_buf.push('\n');
    }

    // Optional appended sections so the AI tool sees importance + relationships, not just
    // the summary tree. Both reuse the existing store data; scoped to the exported roots.
    if include_weights {
        let weights = store.list_weights(None).unwrap_or_default();
        out_buf.push_str(&render_weights(&weights, &format));
    }
    if include_graph {
        // One root → scope the graph to it; multiple/none → whole index. Cap the edges.
        let scope = if roots.len() == 1 {
            roots[0].clone()
        } else {
            "/".to_owned()
        };
        if let Ok(graph) = store.code_graph(&scope, 500, false) {
            out_buf.push_str(&render_graph(&graph, &format));
        }
    }

    if let Some(path) = output {
        // Give an actionable hint when the parent directory doesn't exist, rather
        // than surfacing a bare OS "No such file or directory" error.
        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                anyhow::bail!(
                    "cannot write to '{path}': the directory '{}' does not exist. \
                     Create it first or choose an existing output path.",
                    parent.display()
                );
            }
        }
        std::fs::write(&path, &out_buf).with_context(|| format!("writing export to '{path}'"))?;
        println!("Wrote {} bytes to {path}.", out_buf.len());
    } else {
        print!("{out_buf}");
    }

    Ok(())
}
