use anyhow::Result;
use indexa_core::{config, store::Store};
use std::collections::BTreeMap;
use std::io::IsTerminal;

use super::helpers::require_index_db;

/// One detected application/structure type, aggregated across the directories it matched.
struct Group {
    name: String,
    family: String,
    description: String,
    paths: Vec<String>,
}

pub(crate) async fn cmd_fingerprint(show_paths: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;

    // Prefer the PERSISTED detections (what `ask`/`inspect`/MCP also see, written by the index
    // detector pass). Fall back to a live compute when the table is empty — e.g. only `scan` ran,
    // or the index predates v0.66 — so `indexa fingerprint` still works without a full re-index.
    let persisted = store.all_detected_apps()?;
    let mut groups: Vec<Group> = if !persisted.is_empty() {
        let mut by_kind: BTreeMap<String, Group> = BTreeMap::new();
        for a in persisted {
            by_kind
                .entry(a.app_kind.clone())
                .or_insert_with(|| Group {
                    name: a.app_name.clone(),
                    family: a.family.clone(),
                    description: String::new(),
                    paths: Vec::new(),
                })
                .paths
                .push(a.path);
        }
        by_kind.into_values().collect()
    } else {
        let entry_paths = store.all_entry_paths()?;
        if entry_paths.is_empty() {
            println!("No indexed entries. Run `indexa scan <path>` first.");
            return Ok(());
        }
        // Optional user-extended catalog lives next to the config file.
        let user_fp = config::default_config_path()
            .parent()
            .map(|d| d.join("fingerprints.json"));
        let defs = indexa_core::fingerprint::load(user_fp.as_deref())?;
        indexa_core::fingerprint::detect(entry_paths, &defs)
            .into_iter()
            .map(|d| Group {
                name: d.name,
                family: d.family,
                description: d.description,
                paths: d.paths,
            })
            .collect()
    };

    if groups.is_empty() {
        println!("No software or project fingerprints detected in the index.");
        return Ok(());
    }
    groups.sort_by(|a, b| b.paths.len().cmp(&a.paths.len()).then(a.name.cmp(&b.name)));

    let color = std::io::stdout().is_terminal();
    let bold = |s: &str| {
        if color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };
    let dim = |s: &str| {
        if color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };

    println!(
        "{}",
        bold(&format!("Detected {} application type(s):", groups.len()))
    );
    println!();
    for g in &groups {
        let tail = if g.description.is_empty() {
            format!("({})", g.family)
        } else {
            format!("({}) — {}", g.family, g.description)
        };
        println!(
            "{} {}  {}",
            bold(&format!("{:>4}×", g.paths.len())),
            g.name,
            dim(&tail),
        );
        if show_paths {
            let mut paths = g.paths.clone();
            paths.sort();
            for p in &paths {
                println!("        {}", dim(p));
            }
        }
    }
    Ok(())
}
