use anyhow::Result;
use indexa_core::store::Store;
use indexa_query::{
    build_tree, render_graph, render_json, render_markdown, render_signatures, render_weights,
    render_xml,
};

use super::helpers::{finalize_export, require_index_db, ExportSink};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_export(
    paths: Vec<String>,
    format: String,
    depth: Option<usize>,
    output: Option<String>,
    include_weights: bool,
    include_graph: bool,
    signatures: bool,
    token_budget: Option<usize>,
    strict_budget: bool,
    clipboard: bool,
    strip_comments: bool,
    no_redact: bool,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    // The signatures (code-skeleton) view reads chunks, not summaries, so it works even on an
    // index that has only been `deep`-scanned (no summaries yet).
    if !signatures {
        let count = store.summary_count()?;
        if count == 0 {
            println!("No summaries found. Run `indexa summarize <path>` first.");
            return Ok(());
        }
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
    if signatures {
        // Code-skeleton view: per code file, symbol signatures with bodies elided.
        for root_path in &roots {
            let chunks = store.code_chunks_under(root_path, 0)?;
            if chunks.is_empty() {
                eprintln!(
                    "No indexed code under {root_path} — run `indexa deep {root_path}` first \
                     (or drop --signatures for the summary export)."
                );
                continue;
            }
            out_buf.push_str(&render_signatures(&chunks, &format, !strip_comments));
            out_buf.push('\n');
        }
    } else {
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

    finalize_export(
        out_buf,
        ExportSink {
            redact: !no_redact,
            token_budget,
            strict_budget,
            clipboard,
            output,
        },
    )
}
