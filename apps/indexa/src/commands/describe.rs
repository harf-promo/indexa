use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::require_index_db;

pub(crate) async fn cmd_describe(path: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    // No path → whole-project overview from directory roll-up summaries.
    if path.is_none() {
        let store = Store::open(&db_path)?;
        // Use the first indexed root as the scope so build_project_overview can find the
        // root directory summary (the function needs either hits or an explicit scope).
        let roots = store.root_paths().unwrap_or_default();
        let scope = roots.first().map(|s| s.as_str());
        let overview = indexa_query::build_project_overview(&store, &[], scope, 4000);
        if overview.is_empty() {
            println!("No project overview available. Run `indexa summarize` first to build directory roll-ups.");
        } else {
            println!("{overview}");
        }
        return Ok(());
    }

    let expanded = shellexpand::tilde(path.as_deref().unwrap()).into_owned();
    let store = Store::open(&db_path)?;

    match store.summary_by_path(&expanded)? {
        None => println!("No summary found for {expanded}. Run `indexa summarize` first."),
        Some(rec) => {
            // Print breadcrumb chain
            let crumbs = store.ancestor_summaries(&expanded)?;
            if !crumbs.is_empty() {
                let chain: Vec<&str> = crumbs.iter().map(|c| c.path.as_str()).collect();
                println!("Breadcrumb: {}", chain.join(" › "));
                println!();
                for crumb in &crumbs {
                    let name = std::path::Path::new(&crumb.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| crumb.path.clone());
                    println!("  {name}: {}", crumb.summary);
                }
                println!();
            }

            let kind_icon = if rec.kind == "dir" { "📁" } else { "📄" };
            println!("{kind_icon} {expanded}");
            println!("  Model:  {}", rec.model);
            println!("  Kind:   {}", rec.kind);
            if let Some(ref abstract_) = rec.summary_l0 {
                println!("  Abstract: {abstract_}");
            }
            println!();
            println!("{}", rec.summary);

            // Show immediate children if directory
            if rec.kind == "dir" {
                let children = store.children_summaries(&expanded)?;
                if !children.is_empty() {
                    println!("\nChildren ({}):", children.len());
                    for child in children.iter().take(20) {
                        let name = std::path::Path::new(&child.path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| child.path.clone());
                        let icon = if child.kind == "dir" { "📁" } else { "📄" };
                        println!("  {icon} {name}: {}", child.summary);
                    }
                }
            }
        }
    }

    Ok(())
}
