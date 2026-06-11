use anyhow::Result;
use indexa_core::config::Config;
use indexa_core::decisions::detectors::{classification_fingerprint, UNCERTAINTY_FLOOR};
use indexa_core::decisions::DecisionType;
use indexa_core::smart_classify::{classify_dir_tier0, SemanticCategory};
use indexa_core::store::{ClassificationRecord, NewDecision, Store};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;

use super::helpers::require_index_db;

/// `indexa classify` — Tier 0 (deterministic, content-free) semantic
/// classification of every folder in the index. Auto-suggestions are saved to the
/// `classifications` table. The store already preserves user confirmations and
/// dismissals across runs; the surface to make them (web UI / CLI) lands in a
/// later PR of the Smart-classification series.
pub(crate) async fn cmd_classify(
    show_paths: bool,
    category: Option<String>,
    cfg: &Config,
) -> Result<()> {
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
    // ── Decision Ledger hooks (v0.22) ──────────────────────────────────────────
    // One-time: import pre-ledger user answers so the re-ask detector has priors.
    store.backfill_classification_decisions()?;

    let review = &cfg.review;
    let by_path: HashMap<String, ClassificationRecord> = store
        .list_classifications(None, 0)?
        .into_iter()
        .map(|c| (c.path.clone(), c))
        .collect();
    let options = serde_json::json!(SemanticCategory::ALL
        .iter()
        .map(|c| c.as_str())
        .chain(std::iter::once("ignore"))
        .collect::<Vec<_>>());
    let fp_for = |path: &String| {
        classification_fingerprint(
            own_hint.get(path).and_then(|h| h.as_deref()),
            child_hints.get(path).map(Vec::as_slice).unwrap_or(&[]),
        )
    };

    let mut opened = 0usize;
    let mut open_budget = (review.max_open as i64 - store.open_decision_count()?).max(0) as usize;
    for (path, _kind, cat, confidence) in &rows {
        if opened >= review.max_new_per_scan || open_budget == 0 {
            break;
        }
        match by_path.get(path).map(|c| c.source.as_str()) {
            // Re-ask: the user answered before, and the folder's evidence has
            // changed enough that the fresh suggestion CONTRADICTS that answer.
            // Both conditions required — a contradiction on unchanged evidence is
            // just Tier-0 disagreeing with the user (their answer stands), and
            // changed evidence that still agrees needs no question.
            Some("user") => {
                let Some(prior) =
                    store.latest_decided(DecisionType::Classification.as_str(), path)?
                else {
                    // Confirmed outside the ledger (post-backfill) — nothing to chain to.
                    continue;
                };
                let fp = fp_for(path);
                let user_chosen = prior.chosen.clone().unwrap_or_default();
                // '' = backfilled pre-ledger answer with no recorded fingerprint:
                // the first contradiction is material by definition.
                let evidence_changed = prior.evidence_hash.is_empty() || prior.evidence_hash != fp;
                if evidence_changed && *cat != user_chosen {
                    let opened_id = store.supersede_with(
                        prior.id,
                        NewDecision {
                            decision_type: DecisionType::Classification.as_str().to_owned(),
                            subject: path.clone(),
                            params: serde_json::json!({
                                "category": cat,
                                "confidence": confidence,
                                "prior": {"chosen": user_chosen, "decided_at": prior.decided_at},
                            }),
                            options: options.clone(),
                            auto_value: Some(cat.clone()),
                            confidence: Some(*confidence),
                            evidence_hash: fp,
                            priority: 100,
                            paths: vec![path.clone()],
                        },
                    )?;
                    if opened_id.is_some() {
                        opened += 1;
                        open_budget -= 1;
                    }
                }
            }
            // Sticky tombstone: a dismissed folder is never re-raised here.
            Some("ignored") => {}
            // Uncertainty: mid-band confidence becomes a question instead of a
            // silently-applied label. Confident results stay out of the ledger.
            _ => {
                if *confidence >= UNCERTAINTY_FLOOR && *confidence < review.auto_record_below {
                    let opened_id = store.record_decision(NewDecision {
                        decision_type: DecisionType::Classification.as_str().to_owned(),
                        subject: path.clone(),
                        params: serde_json::json!({"category": cat, "confidence": confidence}),
                        options: options.clone(),
                        auto_value: Some(cat.clone()),
                        confidence: Some(*confidence),
                        evidence_hash: fp_for(path),
                        priority: 50,
                        paths: vec![path.clone()],
                    })?;
                    if opened_id.is_some() {
                        opened += 1;
                        open_budget -= 1;
                    }
                }
            }
        }
    }
    let print_inbox_note = || {
        if opened > 0 {
            println!();
            println!("{opened} question(s) added to the review inbox — see: indexa review list");
        }
    };

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
        print_inbox_note();
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
    print_inbox_note();
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
