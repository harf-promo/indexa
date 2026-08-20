use anyhow::Result;
use indexa_core::store::Store;

use super::helpers::require_index_db;

/// `indexa related <file>` — files related to `file` via the call graph (it calls into
/// them, or they call into it), ranked by shared-symbol count. Reuses the scoped code
/// graph; no LLM. Each result shows its resolution tier (same-file / import / same-dir
/// are structural or proximity-backed; bare is approximate name-only matching).
pub(crate) async fn cmd_related(
    path: String,
    limit: usize,
    json: bool,
    include_co_change: bool,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let target = shellexpand::tilde(&path).into_owned();
    let related = store.find_related_files_resolved(&target, limit)?;
    let co_change = if include_co_change {
        store.co_change_for(&target, limit)?
    } else {
        Vec::new()
    };

    if json {
        let out = serde_json::json!({
            "related": related.iter().map(|r| {
                serde_json::json!({ "path": r.path, "shared": r.shared, "tier": r.tier.as_str() })
            }).collect::<Vec<_>>(),
            "co_change": co_change.iter().map(|c| {
                serde_json::json!({ "path": c.path, "count": c.count })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if related.is_empty() && co_change.is_empty() {
        println!("No related files for \"{target}\".");
        println!(
            "(Needs a deep-indexed code file with call/define edges. Try `indexa deep` first.)"
        );
        return Ok(());
    }
    if !related.is_empty() {
        println!("Files related to {target} (by shared call↔define symbols):");
        println!("{:>9}  {:>7}  Path", "Tier", "Shared");
        println!("{}", "─".repeat(70));
        for r in &related {
            println!("{:>9}  {:>7}  {}", r.tier.label(), r.shared, r.path);
        }
        if related.iter().any(|r| r.tier.is_bare()) {
            println!();
            println!("  bare = name-only match (approximate); same-file/import are structural.");
        }
    }
    if include_co_change {
        if !related.is_empty() {
            println!();
        }
        if co_change.is_empty() {
            println!(
                "No co-change history for {target} (run `indexa graph --compute-co-change` first)."
            );
        } else {
            println!("Files that historically changed together with {target} (git history):");
            for c in &co_change {
                println!("{:>9}  {}", c.count, c.path);
            }
        }
    }
    Ok(())
}
