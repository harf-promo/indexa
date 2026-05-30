use anyhow::Result;
use indexa_core::{config, store::Store};
use std::io::IsTerminal;

use super::helpers::require_index_db;

pub(crate) async fn cmd_fingerprint(show_paths: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
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
    let detections = indexa_core::fingerprint::detect(entry_paths, &defs);

    if detections.is_empty() {
        println!("No software or project fingerprints detected in the index.");
        return Ok(());
    }

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
        bold(&format!(
            "Detected {} fingerprint type(s):",
            detections.len()
        ))
    );
    println!();
    for d in &detections {
        println!(
            "{} {}  {}",
            bold(&format!("{:>4}×", d.paths.len())),
            d.name,
            dim(&format!("({}) — {}", d.category, d.description)),
        );
        if show_paths {
            for p in &d.paths {
                println!("        {}", dim(p));
            }
        }
    }
    Ok(())
}
