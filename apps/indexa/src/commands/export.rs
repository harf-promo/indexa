use anyhow::{bail, Result};
use indexa_core::config::parse_reindex_interval;
use indexa_core::store::Store;
use indexa_query::{
    build_tree, prune_tree, render_graph, render_json, render_markdown, render_signatures,
    render_weights, render_xml,
};
use std::collections::HashSet;

use super::helpers::{finalize_export, index_db_path, ExportSink};

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
    changed_since: Option<String>,
    category: Option<String>,
) -> Result<()> {
    // Export must produce a valid context artifact or fail loudly. A silent success that
    // writes "No index found" into a piped file (`export > ctx.xml`) is exactly the kind of
    // honesty break this project guards against — so error to stderr + non-zero exit instead.
    let db_path = index_db_path()?;
    if !db_path.exists() {
        bail!("No index found. Run `indexa index <path>` first.");
    }
    let store = Store::open(&db_path)?;
    // The signatures (code-skeleton) view reads chunks, not summaries, so it works even on an
    // index that has only been `deep`-scanned (no summaries yet).
    if !signatures && store.summary_count()? == 0 {
        bail!("No summaries found. Run `indexa summarize <path>` first.");
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

    let now_secs: i64 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    let now = now_secs.to_string();

    // Relational slice (v0.58): restrict the export to files matching --changed-since and/or
    // --category. `None` ⇒ no filter (export everything); `Some(set)` ⇒ only these file paths.
    let allow: Option<HashSet<String>> = build_export_filter(
        &store,
        changed_since.as_deref(),
        category.as_deref(),
        now_secs,
    )?;

    let mut out_buf = String::new();
    if signatures {
        // Code-skeleton view: per code file, symbol signatures with bodies elided.
        for root_path in &roots {
            let mut chunks = store.code_chunks_under(root_path, 0)?;
            if let Some(allow) = &allow {
                chunks.retain(|c| allow.contains(&c.entry_path));
            }
            if chunks.is_empty() {
                eprintln!(
                    "No indexed code under {root_path} matched — run `indexa deep {root_path}` \
                     first, drop --signatures, or widen the slice filter."
                );
                continue;
            }
            out_buf.push_str(&render_signatures(&chunks, &format, !strip_comments));
            out_buf.push('\n');
        }
    } else {
        for root_path in &roots {
            let Some(tree) = build_tree(&store, root_path, depth)? else {
                eprintln!(
                    "No summary found for {root_path} — run `indexa summarize {root_path}` first."
                );
                continue;
            };
            // Apply the relational slice: prune to matched files + the directories on the
            // path to them. A root with no match is skipped.
            let tree = match &allow {
                Some(allow) => match prune_tree(tree, allow) {
                    Some(t) => t,
                    None => continue,
                },
                None => tree,
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

    // Honesty guard: never emit an empty (but exit-0) artifact. Most relevant when a slice
    // filter matched no files — a piped consumer must see a failure, not silence.
    if out_buf.trim().is_empty() {
        if allow.is_some() {
            bail!(
                "Nothing matched the export slice (--changed-since / --category). \
                 Widen the window/category or drop the filter."
            );
        }
        bail!("Nothing to export for the requested path(s). Run `indexa summarize <path>` first.");
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

/// Build the relational-slice allow-set for [`cmd_export`]. Returns `None` when no slice
/// flag is given (export everything), or `Some(set)` of the file paths passing the
/// filter(s). With both `--changed-since` and `--category`, the result is their
/// intersection. Reuses [`parse_reindex_interval`] for the duration grammar (`7d`/`12h`/…)
/// and the classifications table for categories — neither triggers a re-scan.
fn build_export_filter(
    store: &Store,
    changed_since: Option<&str>,
    category: Option<&str>,
    now_secs: i64,
) -> Result<Option<HashSet<String>>> {
    let mut allow: Option<HashSet<String>> = None;

    if let Some(dur) = changed_since {
        let secs = parse_reindex_interval(dur).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --changed-since '{dur}': use a window like 7d, 12h, 90m, or 3600s"
            )
        })?;
        let cutoff = now_secs - secs as i64;
        let set: HashSet<String> = store.paths_modified_since(cutoff)?.into_iter().collect();
        allow = Some(set);
    }

    if let Some(cat) = category {
        // Skip `ignored` rows: a file the user explicitly dismissed from this category keeps
        // its old `category` as a tombstone, but it must NOT be pulled into the slice — that
        // would contradict the user's own judgment about what belongs.
        let set: HashSet<String> = store
            .classifications_in_category(cat, 0)?
            .into_iter()
            .filter(|c| c.source != "ignored")
            .map(|c| c.path)
            .collect();
        allow = Some(match allow {
            Some(prev) => prev.intersection(&set).cloned().collect(),
            None => set,
        });
    }

    Ok(allow)
}
