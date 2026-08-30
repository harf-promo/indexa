use anyhow::Result;
use indexa_core::{config::Config, store::Store};

use super::helpers::require_index_db;

pub(crate) async fn cmd_weight_set(
    target: String,
    weight: f32,
    kind: String,
    _cfg: &Config,
) -> Result<()> {
    if weight < 0.0 {
        anyhow::bail!("weight must be ≥ 0.0");
    }
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;

    // Auto-detect kind from path if not explicitly specified.
    let looks_like_path = target.contains('/') || target.contains('\\');
    let resolved_kind = if kind == "auto" {
        let p = std::path::Path::new(&target);
        if p.is_dir() {
            "dir"
        } else if p.is_file() {
            "file"
        } else {
            // Treat as category if it doesn't look like a path.
            if looks_like_path {
                "file"
            } else {
                "category"
            }
        }
    } else {
        kind.as_str()
    };

    // Warn (don't block) if a path-like target doesn't exist on disk: the weight is still
    // stored and will activate if the path is created later, but a typo is the likely cause.
    if (resolved_kind == "file" || resolved_kind == "dir")
        && looks_like_path
        && !std::path::Path::new(&target).exists()
    {
        eprintln!(
            "  ⚠  \"{target}\" does not exist on disk — storing the weight anyway \
             (it will apply if the path is created). Check for a typo."
        );
    }

    store.set_weight(resolved_kind, &target, weight, "user", None)?;
    println!("Set {resolved_kind} weight for \"{target}\" = {weight:.2}");
    Ok(())
}

pub(crate) async fn cmd_weight_get(path: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let w = store.weight_for(&path)?;
    println!("Resolved weight for \"{path}\": {w:.3}");
    Ok(())
}

pub(crate) async fn cmd_weight_list(kind: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let weights = store.list_weights(kind.as_deref())?;
    if weights.is_empty() {
        println!("No importance weights set. Use `indexa weight set <path> <value>` to add one.");
        return Ok(());
    }
    println!("{:<8} {:<10} {:>8}  Target", "Kind", "Source", "Weight");
    println!("{}", "─".repeat(70));
    for w in &weights {
        let reason = w
            .reason
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        println!(
            "{:<8} {:<10} {:>8.3}  {}{}",
            w.target_kind, w.source, w.weight, w.target, reason
        );
    }
    Ok(())
}

pub(crate) async fn cmd_weight_delete(target: String, kind: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;

    let resolved_kind = kind.clone().unwrap_or_else(|| {
        if target.contains('/') || target.contains('\\') {
            "file".to_owned()
        } else {
            "category".to_owned()
        }
    });

    let mut deleted = false;
    for k in kinds_to_delete(&resolved_kind, kind.is_some()) {
        store.delete_weight(k, &target)?;
        deleted = true;
    }
    if deleted {
        println!("Deleted weight for \"{target}\".");
    } else {
        println!("No weight found for \"{target}\" (kind={resolved_kind}).");
    }
    Ok(())
}

/// Which weight kinds `cmd_weight_delete` should try, given the resolved kind string and whether
/// `--kind` was passed explicitly by the user.
///
/// When `--kind` was **not** given, `resolved_kind` came from auto-detection off the target
/// string alone, which can't distinguish a file from a directory (both look like paths) — so a
/// `"file"` auto-guess also tries `"dir"`, same as before. But when the user passed `--kind`
/// **explicitly** (e.g. `--kind file`), that widening must NOT apply: `indexa weight delete
/// <path> --kind file` must delete only the file-kind weight, never silently also delete an
/// unrelated dir-kind weight for the same path.
fn kinds_to_delete(resolved_kind: &str, explicit: bool) -> Vec<&'static str> {
    ["file", "dir", "category"]
        .into_iter()
        .filter(|&k| resolved_kind == k || (!explicit && resolved_kind == "file" && k == "dir"))
        .collect()
}

pub(crate) async fn cmd_weight_suggest(days: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let suggestions = store.suggest_weights_by_recency(days)?;
    if suggestions.is_empty() {
        println!("No files modified in the last {days} days found.");
        return Ok(());
    }
    println!("Recency-based weight suggestions (files modified in last {days} days):");
    println!("{:>8}  Path", "Weight");
    println!("{}", "─".repeat(60));
    for (path, w) in &suggestions {
        println!("{:>8.2}  {path}", w);
    }
    println!("\nRun `indexa weight apply --days {days}` to apply these weights.");
    Ok(())
}

pub(crate) async fn cmd_weight_apply(days: i64, yes: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let suggestions = store.suggest_weights_by_recency(days)?;
    if suggestions.is_empty() {
        println!("No recency-based suggestions for the last {days} days.");
        return Ok(());
    }
    println!("Will apply {} recency weight(s):", suggestions.len());
    for (path, w) in &suggestions {
        println!("  {:.2}  {path}", w);
    }
    if !yes {
        use std::io::IsTerminal as _;
        if std::io::stdin().is_terminal() {
            print!("\nApply these {} weights? [y/N] ", suggestions.len());
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if input.trim().to_lowercase() != "y" {
                println!("Aborted.");
                return Ok(());
            }
        }
    }
    for (path, w) in &suggestions {
        // Determine kind.
        let p = std::path::Path::new(path);
        let kind = if p.is_dir() { "dir" } else { "file" };
        store.set_weight(kind, path, *w, "auto", Some("recency"))?;
    }
    println!("Applied {} importance weight(s).", suggestions.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Auto-detected (no `--kind` given) `"file"` is genuinely ambiguous — a path-like target
    /// could be either — so both `file` and `dir` are tried, same as before this fix.
    #[test]
    fn auto_detected_file_kind_also_tries_dir() {
        let mut kinds = kinds_to_delete("file", false);
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["dir", "file"]);
    }

    /// Regression test: an EXPLICIT `--kind file` must delete only the file-kind weight — it must
    /// NOT also silently delete an unrelated dir-kind weight for the same target.
    #[test]
    fn explicit_file_kind_does_not_also_delete_dir() {
        assert_eq!(kinds_to_delete("file", true), vec!["file"]);
    }

    #[test]
    fn dir_kind_never_widens_to_file_explicit_or_not() {
        assert_eq!(kinds_to_delete("dir", false), vec!["dir"]);
        assert_eq!(kinds_to_delete("dir", true), vec!["dir"]);
    }

    #[test]
    fn category_kind_is_exact_explicit_or_not() {
        assert_eq!(kinds_to_delete("category", false), vec!["category"]);
        assert_eq!(kinds_to_delete("category", true), vec!["category"]);
    }
}
