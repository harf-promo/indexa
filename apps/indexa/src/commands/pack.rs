use anyhow::{bail, Result};
use indexa_core::store::Store;
use indexa_query::{build_tree, render_json, render_markdown, render_xml};

use super::helpers::require_index_db;

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

pub(crate) async fn cmd_pack_create(name: String, description: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let id = store.create_pack(&name, description.as_deref())?;
    println!("Created pack \"{name}\" (id: {id})");
    println!("Add paths with: indexa pack add \"{name}\" <paths…>");
    Ok(())
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

pub(crate) async fn cmd_pack_export(
    name: String,
    format: String,
    output: Option<String>,
    depth: Option<usize>,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
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

    if is_xml {
        out_buf.push_str("</context>\n");
    }

    if exported == 0 {
        bail!(
            "No paths in pack \"{name}\" have summaries yet. \
             Run `indexa summarize <path>` or `indexa index <path>` first."
        );
    }

    if let Some(out_path) = output {
        if let Some(parent) = std::path::Path::new(&out_path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                bail!(
                    "cannot write to '{out_path}': directory '{}' does not exist.",
                    parent.display()
                );
            }
        }
        std::fs::write(&out_path, &out_buf)?;
        println!(
            "Exported pack \"{name}\" ({exported}/{} paths) to {out_path}.",
            paths.len()
        );
    } else {
        print!("{out_buf}");
    }

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
