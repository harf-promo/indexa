use anyhow::Result;
use indexa_core::smart_classify::{classify_dir_tier0, SemanticCategory};
use indexa_core::store::{ClassificationRecord, Store};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;

use super::helpers::require_index_db;

/// `indexa classify` — Tier 0 (deterministic, content-free) semantic
/// classification of every folder in the index. Auto-suggestions are saved to the
/// `classifications` table. The store already preserves user confirmations and
/// dismissals across runs; the surface to make them (web UI / CLI) lands in a
/// later PR of the Smart-classification series.
pub(crate) async fn cmd_classify(show_paths: bool, category: Option<String>) -> Result<()> {
    if let Some(c) = &category {
        if SemanticCategory::parse(c).is_none() {
            anyhow::bail!(
                "unknown --category '{c}'. Valid: work, personal, archive, media, code, system, other"
            );
        }
    }

    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    if store.entry_count()? == 0 {
        println!("No indexed entries. Run `indexa scan <path>` first.");
        return Ok(());
    }

    // ── Tier 0: own surface hint, else dominant child-file category ────────────
    let dir_entries = store.dir_entries_with_hint()?;
    let histogram = store.child_file_hint_histogram()?;

    let mut child_hints: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    for (parent, hint_cat, count) in histogram {
        child_hints
            .entry(parent)
            .or_default()
            .push((hint_cat, count));
    }

    let mut own_hint: HashMap<String, Option<String>> = HashMap::new();
    let mut dirs: HashSet<String> = HashSet::new();
    for (path, hint_cat) in dir_entries {
        dirs.insert(path.clone());
        own_hint.insert(path, hint_cat);
    }
    dirs.extend(child_hints.keys().cloned());

    let no_children: Vec<(String, i64)> = Vec::new();
    let mut rows: Vec<(String, String, String, f32)> = Vec::new();
    for path in &dirs {
        let hint = own_hint.get(path).and_then(|h| h.as_deref());
        let children = child_hints.get(path).unwrap_or(&no_children);
        if let Some((cat, confidence)) = classify_dir_tier0(hint, children) {
            rows.push((
                path.clone(),
                "dir".to_owned(),
                cat.as_str().to_owned(),
                confidence,
            ));
        }
    }
    store.upsert_auto_classifications(&rows)?;

    // ── Render the saved state (auto + user; tombstoned `ignored` excluded) ────
    let mut records: Vec<ClassificationRecord> = store
        .list_classifications(None, 0)?
        .into_iter()
        .filter(|c| c.source != "ignored")
        .collect();
    if let Some(c) = &category {
        records.retain(|r| &r.category == c);
    }

    let color = std::io::stdout().is_terminal();
    let sgr = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };

    if records.is_empty() {
        if category.is_some() {
            println!("No folders classified as that category yet.");
        } else {
            println!(
                "No folders could be classified from surface hints yet. work/personal need \
content — run `indexa deep` + `indexa summarize`, then a later release can infer them."
            );
        }
        return Ok(());
    }

    let confirmed = records.iter().filter(|r| r.source == "user").count();
    let suggested = records.iter().filter(|r| r.source == "auto").count();

    let mut by_cat: HashMap<String, Vec<ClassificationRecord>> = HashMap::new();
    for r in records {
        by_cat.entry(r.category.clone()).or_default().push(r);
    }
    let mut cats: Vec<String> = by_cat.keys().cloned().collect();
    cats.sort();

    println!(
        "{}",
        sgr(
            "1",
            &format!(
                "Smart classification — {} folder(s): {suggested} suggested, {confirmed} confirmed",
                suggested + confirmed
            )
        )
    );
    println!();

    for cat in &cats {
        let recs = &by_cat[cat];
        println!(
            "{} {}",
            sgr(category_color(cat), &format!("{cat:<10}")),
            sgr("2", &format!("{} folder(s)", recs.len())),
        );
        if show_paths {
            let mut sorted = recs.clone();
            sorted.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.path.cmp(&b.path))
            });
            for r in &sorted {
                let tag = if r.source == "user" {
                    "✓ confirmed"
                } else {
                    "· suggested"
                };
                println!(
                    "    {}  {}",
                    sgr("2", &format!("{tag}  {:>3.0}%", r.confidence * 100.0)),
                    r.path,
                );
            }
        }
    }

    println!();
    println!(
        "{}",
        sgr(
            "2",
            "Folders needing content to tell work from personal stay pending until deeper \
inference. Suggestions are saved; confirming or correcting them lands in an upcoming release."
        )
    );
    Ok(())
}

/// ANSI SGR color for a semantic category.
fn category_color(category: &str) -> &'static str {
    match category {
        "code" => "36",     // cyan
        "work" => "34",     // blue
        "personal" => "35", // magenta
        "media" => "33",    // yellow
        "archive" => "31",  // red
        "system" => "90",   // bright black
        _ => "37",          // white (other)
    }
}
