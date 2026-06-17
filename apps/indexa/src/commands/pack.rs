use anyhow::{bail, Result};
use indexa_core::{
    config::{Config, HybridMode},
    store::Store,
};
use indexa_query::{build_tree, render_json, render_markdown, render_xml};

use super::helpers::{
    build_embedder, finalize_export, index_db_path, require_index_db, ExportSink,
};

/// Resolve and expand a path, canonicalizing `~` prefixes.
fn expand(p: &str) -> String {
    shellexpand::tilde(p).into_owned()
}

fn now_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

pub(crate) async fn cmd_pack_create(
    name: String,
    description: Option<String>,
    auto: bool,
    yes: bool,
    limit: usize,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let id = store.create_pack(&name, description.as_deref())?;
    println!("Created pack \"{name}\" (id: {id})");

    if !auto {
        println!("Add paths with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }

    // ── Auto-suggest paths ────────────────────────────────────────────────────
    println!("Searching for paths related to \"{name}\"…");

    // Try semantic search (requires embedder + summarised tree with embeddings).
    let candidates: Vec<String> = match build_embedder(cfg, None) {
        Ok(embedder) => match embedder.embed(&name).await {
            Ok(embedding) => {
                let hits = store.summary_cosine_search(&embedding, limit, 0.15)?;
                if hits.is_empty() {
                    eprintln!("  (no summary embeddings found — falling back to keyword search)");
                    keyword_suggest(&store, &name, limit)?
                } else {
                    println!("  [semantic match — {} candidates]", hits.len());
                    hits.into_iter().map(|(path, _score)| path).collect()
                }
            }
            Err(e) => {
                eprintln!("  (embedding failed: {e:#} — falling back to keyword search)");
                keyword_suggest(&store, &name, limit)?
            }
        },
        Err(e) => {
            eprintln!("  (embedder unavailable: {e:#} — falling back to keyword search)");
            keyword_suggest(&store, &name, limit)?
        }
    };

    if candidates.is_empty() {
        println!("No related paths found. Add manually with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }

    println!("\nSuggested paths ({}):", candidates.len());
    for p in &candidates {
        println!("  {p}");
    }

    // ── Confirm ───────────────────────────────────────────────────────────────
    let confirmed = if yes {
        true
    } else {
        use std::io::IsTerminal as _;
        if std::io::stdin().is_terminal() {
            print!(
                "\nAdd all {} paths to pack \"{name}\"? [Y/n] ",
                candidates.len()
            );
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().is_empty() || input.trim().to_lowercase() == "y"
        } else {
            true // non-interactive: accept
        }
    };

    if !confirmed {
        println!("Skipped. Add manually with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }

    store.add_pack_paths(&id, &candidates)?;
    println!(
        "Added {} path{} to \"{name}\".",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Keyword fallback for `--auto` when embeddings are unavailable.
fn keyword_suggest(store: &Store, query: &str, limit: usize) -> Result<Vec<String>> {
    let hits = store.hybrid_search(query, None, &HybridMode::Sparse, None, limit * 3, 0.0)?;
    println!("  [keyword match — {} chunk hits]", hits.len());
    let mut seen = std::collections::HashSet::new();
    let paths: Vec<String> = hits
        .into_iter()
        .filter_map(|h| {
            if seen.insert(h.entry_path.clone()) {
                Some(h.entry_path)
            } else {
                None
            }
        })
        .take(limit)
        .collect();
    Ok(paths)
}

pub(crate) async fn cmd_pack_add(name: String, paths: Vec<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store.pack_by_name(&name)?.ok_or_else(|| {
        anyhow::anyhow!("no pack named \"{name}\" — create it first with `indexa pack create`")
    })?;
    let expanded: Vec<String> = paths.iter().map(|p| expand(p)).collect();
    store.add_pack_paths(&pack.id, &expanded)?;
    println!(
        "Added {} path{} to \"{}\".",
        expanded.len(),
        if expanded.len() == 1 { "" } else { "s" },
        name
    );
    Ok(())
}

/// Fetch a remote source (GitHub issue/PR or web page), cache it as a local Markdown file, and add
/// that file to the pack. Network access is gated by `[sources] enabled` / `INDEXA_REMOTE_FETCH_ALLOW`.
pub(crate) async fn cmd_pack_add_url(
    name: String,
    url: String,
    label: Option<String>,
    cfg: &Config,
) -> Result<()> {
    use super::sources;

    if !sources::remote_fetch_allowed(&cfg.sources) {
        bail!(
            "Remote fetching is off. Enable it with `[sources]\\nenabled = true` in config.toml, \
             or set INDEXA_REMOTE_FETCH_ALLOW=1 for this run. (Fetching reaches the network, so \
             it's opt-in.)"
        );
    }
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store.pack_by_name(&name)?.ok_or_else(|| {
        anyhow::anyhow!("no pack named \"{name}\" — create it first with `indexa pack create`")
    })?;

    println!("Fetching {url} …");
    let md = sources::fetch_source_markdown(&url, &cfg.sources).await?;
    let data_dir = indexa_core::config::default_data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    let path = sources::cache_source(&data_dir, &url, label.as_deref(), &md)?;
    let path_str = path.to_string_lossy().into_owned();
    store.add_pack_paths(&pack.id, std::slice::from_ref(&path_str))?;

    println!("Cached {} bytes → {path_str}", md.len());
    println!(
        "Added to pack \"{name}\". Run `indexa index \"{path_str}\"` to make it searchable, \
         then `indexa pack export \"{name}\"`."
    );
    Ok(())
}

pub(crate) async fn cmd_pack_remove(name: String, paths: Vec<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let expanded: Vec<String> = paths.iter().map(|p| expand(p)).collect();
    store.remove_pack_paths(&pack.id, &expanded)?;
    println!("Removed {} path(s) from \"{}\".", expanded.len(), name);
    Ok(())
}

pub(crate) async fn cmd_pack_list() -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let packs = store.list_packs()?;
    if packs.is_empty() {
        println!("No Context Packs yet.");
        println!("Create one with: indexa pack create \"<name>\"");
        return Ok(());
    }
    println!("{:<20} {:>6}  Description", "Name", "Paths");
    println!("{}", "─".repeat(60));
    for p in &packs {
        let desc = p.description.as_deref().unwrap_or("—");
        println!("{:<20} {:>6}  {}", p.name, p.path_count, desc);
    }
    Ok(())
}

pub(crate) async fn cmd_pack_show(name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let paths = store.pack_paths(&pack.id)?;
    if paths.is_empty() {
        println!("Pack \"{name}\" is empty.");
        println!("Add paths with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }
    let desc = pack
        .description
        .as_deref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    println!("Pack \"{name}\"{desc} ({} paths):", paths.len());
    for p in &paths {
        println!("  {p}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_pack_export(
    name: String,
    format: String,
    output: Option<String>,
    depth: Option<usize>,
    include_weights: bool,
    signatures: bool,
    token_budget: Option<usize>,
    strict_budget: bool,
    clipboard: bool,
    strip_comments: bool,
    no_redact: bool,
) -> Result<()> {
    // Like `indexa export`, a pack export must produce a valid artifact or fail loudly — never
    // a silent stdout notice that gets written into a piped file with a zero exit.
    let db_path = index_db_path()?;
    if !db_path.exists() {
        bail!("No index found. Run `indexa index <path>` first.");
    }
    let store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let paths = store.pack_paths(&pack.id)?;
    if paths.is_empty() {
        bail!("Pack \"{name}\" has no paths. Add paths first with `indexa pack add`.");
    }

    let now = now_str();
    let mut out_buf = String::new();
    let is_xml = format != "md" && format != "markdown" && format != "json";

    // XML: wrap all roots in a single <context> element for a self-contained file
    if is_xml {
        out_buf.push_str("<context pack=\"");
        out_buf.push_str(&xml_escape(&name));
        out_buf.push_str("\" generated=\"");
        out_buf.push_str(&now);
        out_buf.push_str("\">\n");
    }

    let mut exported = 0usize;
    for root_path in &paths {
        if signatures {
            // Code-skeleton view (reads chunks; works without summaries).
            let chunks = store.code_chunks_under(root_path, 0)?;
            if chunks.is_empty() {
                eprintln!("  \u{26a0} No indexed code under {root_path} — run `indexa deep {root_path}` first.");
                continue;
            }
            out_buf.push_str(&indexa_query::render_signatures(
                &chunks,
                &format,
                !strip_comments,
            ));
            out_buf.push('\n');
            exported += 1;
            continue;
        }
        let tree = build_tree(&store, root_path, depth)?;
        let Some(tree) = tree else {
            eprintln!(
                "  \u{26a0} No summary for {root_path} \
                 — run `indexa summarize {root_path}` first."
            );
            continue;
        };
        let rendered = match format.as_str() {
            "md" | "markdown" => render_markdown(&tree),
            "json" => render_json(&tree),
            _ => render_xml(&tree, &now),
        };
        out_buf.push_str(&rendered);
        out_buf.push('\n');
        exported += 1;
    }

    // Optional importance-weights section (global; reuses the same renderer as `export`).
    if include_weights {
        out_buf.push_str(&indexa_query::render_weights(
            &store.list_weights(None).unwrap_or_default(),
            &format,
        ));
    }

    if is_xml {
        out_buf.push_str("</context>\n");
    }

    if exported == 0 {
        let hint = if signatures {
            "have indexed code yet. Run `indexa deep <path>` first."
        } else {
            "have summaries yet. Run `indexa summarize <path>` or `indexa index <path>` first."
        };
        bail!("No paths in pack \"{name}\" {hint}");
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

pub(crate) async fn cmd_pack_rename(name: String, new_name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    if store.pack_by_name(&new_name)?.is_some() {
        bail!("a pack named \"{new_name}\" already exists.");
    }
    store.rename_pack(&pack.id, &new_name)?;
    println!("Renamed pack \"{name}\" → \"{new_name}\".");
    Ok(())
}

pub(crate) async fn cmd_pack_delete(name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    store.delete_pack(&pack.id)?;
    println!("Deleted pack \"{name}\". (Indexed files are untouched.)");
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
