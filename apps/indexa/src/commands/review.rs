use anyhow::{anyhow, Result};
use indexa_core::config::Config;
use indexa_core::decisions::templates::render_question;
use indexa_core::decisions::{decide_and_apply, detectors, effects, DecisionType};
use indexa_core::store::{DecisionRecord, Store};
use std::io::IsTerminal;

use super::helpers::{format_unix_timestamp, require_index_db};

/// Hard display bound for `review list`. The detectors already cap the inbox at
/// `[review] max_open` (default 50); this only guards a hand-grown database.
const LIST_LIMIT: usize = 500;

/// `indexa review list` — the open-question inbox, highest priority first.
pub(crate) async fn cmd_review_list(decision_type: Option<String>) -> Result<()> {
    let filter = decision_type.as_deref().map(parse_type).transpose()?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let rows = store.open_decisions(filter.map(DecisionType::as_str), LIST_LIMIT)?;

    if rows.is_empty() {
        match filter {
            Some(t) => println!("No open {t} questions."),
            None => println!(
                "Inbox zero — nothing needs your judgment. (Detectors run during \
indexa index / classify; or: indexa review scan)"
            ),
        }
        return Ok(());
    }

    let color = std::io::stdout().is_terminal();
    let sgr = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };

    println!(
        "{}",
        sgr(
            "1",
            &format!("Review inbox — {} open question(s)", rows.len())
        )
    );
    println!();
    for d in &rows {
        let q = render_question(d);
        println!(
            "  {}  {}  {}",
            sgr("1", &format!("#{}", q.id)),
            sgr("2", &format!("[{}]", q.decision_type)),
            q.title
        );
        let values: Vec<String> = q
            .options
            .iter()
            .enumerate()
            .map(|(i, (v, _))| format!("{}. {v}", i + 1))
            .collect();
        println!(
            "      {}",
            sgr("2", &format!("answers: {}", values.join("  ·  ")))
        );
    }
    println!();
    println!(
        "{}",
        sgr(
            "2",
            "indexa review answer <id> <value or number> — or indexa review show <id> for detail"
        )
    );
    Ok(())
}

/// `indexa review show <id>` — full rendering, the raw evidence behind the
/// question, and the revision chain when this row links to other revisions.
pub(crate) async fn cmd_review_show(id: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let d = store
        .decision_by_id(id)?
        .ok_or_else(|| anyhow!("no decision with id {id}"))?;
    let q = render_question(&d);

    let color = std::io::stdout().is_terminal();
    let sgr = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };

    println!(
        "{} {}",
        sgr("1", &format!("#{}", d.id)),
        sgr(
            "2",
            &format!(
                "{} · {} · priority {}",
                d.decision_type, d.status, d.priority
            )
        ),
    );
    println!("{}", sgr("2", &format!("subject: {}", d.subject)));
    println!();
    println!("  {}", sgr("1", &q.title));
    println!("  {}", q.detail);

    if !q.options.is_empty() {
        let width = q.options.iter().map(|(v, _)| v.len()).max().unwrap_or(0);
        println!();
        println!("{}", sgr("1", "Answers (use the value or its number):"));
        for (i, (value, label)) in q.options.iter().enumerate() {
            let n = i + 1;
            // Self-labeling values (categories, paths) need no second column.
            if label == value {
                println!("  {n}. {value}");
            } else {
                println!("  {n}. {value:<width$}  {}", sgr("2", label));
            }
        }
    }

    // Raw params verbatim, so the evidence behind the question is auditable.
    let params: serde_json::Value =
        serde_json::from_str(&d.params).unwrap_or(serde_json::Value::Null);
    if params.as_object().is_some_and(|o| !o.is_empty()) {
        println!();
        println!("{}", sgr("1", "Evidence (raw params):"));
        for line in serde_json::to_string_pretty(&params)?.lines() {
            println!("  {}", sgr("2", line));
        }
    }

    if let Some(chosen) = &d.chosen {
        println!();
        println!(
            "Answered: {chosen} ({}) on {}",
            d.source.as_deref().unwrap_or("?"),
            format_unix_timestamp(d.decided_at.unwrap_or(d.created_at))
        );
        if let Some(e) = &d.effects {
            println!("{}", sgr("2", &format!("Effects: {e}")));
        }
    }

    let history = store.decision_history(&d.decision_type, &d.subject)?;
    if d.parent_id.is_some() || d.superseded_by.is_some() || history.len() > 1 {
        println!();
        println!("{}", sgr("1", "Revision chain:"));
        for h in &history {
            println!("  {}", history_line(h, &sgr));
        }
    }
    Ok(())
}

/// `indexa review answer` — single (`<id> <choice>`) or batch
/// (`--type T --under DIR --choose V`); clap guarantees the two shapes.
pub(crate) async fn cmd_review_answer(
    id: Option<i64>,
    choice: Option<String>,
    decision_type: Option<String>,
    under: Option<String>,
    choose: Option<String>,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;

    if let (Some(id), Some(choice)) = (id, &choice) {
        // Numeric shorthand: `answer 12 2` picks the question's 2nd listed
        // option — duplicate answers are full paths nobody wants to retype.
        // Safe because option values are categories/paths/keep_all/ignore,
        // never bare integers; a literal-value match always wins regardless.
        let resolved = resolve_choice(&store, id, choice)?;
        // decide_and_apply validates the choice against the row's options
        // BEFORE committing — an off-menu answer leaves the row open.
        let effects = decide_and_apply(&mut store, id, &resolved, "user")?;
        println!("Decision #{id} answered: {resolved}");
        println!("  → {}", effects_summary(&effects));
        return Ok(());
    }

    let (t, under, choose) = match (&decision_type, &under, &choose) {
        (Some(t), Some(u), Some(c)) => (parse_type(t)?, u, c),
        _ => anyhow::bail!("answer needs <id> <choice>, or --type with --under and --choose"),
    };

    // answer_decisions_under can't consult each row's options, so the value is
    // validated per type up front (shared with the web batch endpoint so the
    // batch-safety rules can't drift).
    if let Some(msg) = indexa_core::decisions::batch_answer_refusal(t, choose) {
        anyhow::bail!(msg);
    }

    let dir = shellexpand::tilde(under).into_owned();
    let ids = store.answer_decisions_under(&dir, t.as_str(), choose, "user")?;
    if ids.is_empty() {
        println!("No open {t} questions under {dir}.");
        return Ok(());
    }

    // The answers are committed; project each one (decide_and_apply order). A
    // row whose projection fails is left for the repair sweep, never blocking
    // the rest.
    let mut applied = 0usize;
    for &aid in &ids {
        let Some(d) = store.decision_by_id(aid)? else {
            continue;
        };
        match effects::apply_decision_effects(&mut store, &d) {
            Ok(e) => {
                store.mark_effects_applied(aid, &e)?;
                applied += 1;
                println!("  #{aid}  {} → {}", d.subject, effects_summary(&e));
            }
            Err(e) => eprintln!(
                "  #{aid}  {}: projection failed ({e:#}) — the repair sweep will retry",
                d.subject
            ),
        }
    }
    println!(
        "Answered {applied} of {} question(s) under {dir} with '{choose}'.",
        ids.len()
    );
    Ok(())
}

/// `indexa review dismiss <id>`.
pub(crate) async fn cmd_review_dismiss(id: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    store.dismiss_decision(id)?;
    println!("Decision #{id} dismissed — it only returns if its evidence changes.");
    Ok(())
}

/// `indexa review history <path>` — every recorded decision about a path
/// (both types), oldest first.
pub(crate) async fn cmd_review_history(path: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let expanded = shellexpand::tilde(&path).into_owned();
    // Subjects are stored canonicalized; try the literal form first, then the
    // canonical one, so a symlinked path still finds its history.
    let mut rows = history_for_subject(&store, &expanded)?;
    if rows.is_empty() {
        if let Ok(canon) = std::fs::canonicalize(&expanded) {
            let canon = canon.to_string_lossy().into_owned();
            if canon != expanded {
                rows = history_for_subject(&store, &canon)?;
            }
        }
    }
    if rows.is_empty() {
        println!("No decisions recorded for {expanded}.");
        return Ok(());
    }
    rows.sort_by_key(|d| (d.created_at, d.id));

    let color = std::io::stdout().is_terminal();
    let sgr = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };

    println!(
        "{}",
        sgr(
            "1",
            &format!("Decision history — {expanded} ({} revision(s))", rows.len())
        )
    );
    println!();
    for d in &rows {
        println!("  {}", history_line(d, &sgr));
    }
    Ok(())
}

/// `indexa review revert <id>` — restore a decided revision's answer by
/// appending a new revision (never deletes) and re-running the idempotent
/// projection. Thin shell over `core::decisions::revert_decision`, the single
/// implementation the web's `POST /api/review/revert` shares.
pub(crate) async fn cmd_review_revert(id: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let out = indexa_core::decisions::revert_decision(&mut store, id)
        .map_err(|e| anyhow!("{e:#} (see `indexa review list`)"))?;
    println!(
        "Restored '{}' for {} — revision #{} supersedes #{}.",
        out.chosen, out.subject, out.new_id, out.superseded_id
    );
    println!("  → {}", effects_summary(&out.effects));
    Ok(())
}

/// `indexa review scan` — run the standalone detector pass now (the same one
/// `indexa index` appends).
pub(crate) async fn cmd_review_scan(cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let report = detectors::run_detectors(&mut store, &cfg.review)?;
    println!(
        "Detector pass — {} question(s) opened, {} skipped (already covered or dismissed), \
{} repaired.",
        report.opened, report.skipped, report.repaired
    );
    if report.opened > 0 {
        println!("See them with: indexa review list");
    }
    Ok(())
}

/// `indexa review gc` — drop dismissed/expired rows past the horizon. Live
/// chains are never broken (the store keeps any row another row references).
pub(crate) async fn cmd_review_gc(older_than_days: i64) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let n = store.gc_decisions(older_than_days.saturating_mul(86_400))?;
    println!("Removed {n} dismissed/expired question(s) older than {older_than_days} day(s).");
    Ok(())
}

/// Resolve a numeric answer shorthand to the question's nth listed option
/// (1-based, the numbering `list`/`show` display). A literal option-value
/// match always wins; out-of-range numbers error rather than pass through,
/// since a bare integer can never be a real option value.
fn resolve_choice(store: &Store, id: i64, choice: &str) -> Result<String> {
    let Ok(n) = choice.parse::<usize>() else {
        return Ok(choice.to_owned());
    };
    let Some(d) = store.decision_by_id(id)? else {
        return Ok(choice.to_owned()); // decide_and_apply reports the missing id
    };
    let q = render_question(&d);
    if q.options.iter().any(|(v, _)| v == choice) {
        return Ok(choice.to_owned());
    }
    if n >= 1 && n <= q.options.len() {
        return Ok(q.options[n - 1].0.clone());
    }
    anyhow::bail!(
        "{n} is out of range for decision {id} — it has {} option(s) (see: indexa review show {id})",
        q.options.len()
    )
}

/// Parse a `--type` value with the valid set spelled out, like classify's
/// `--category` validation.
fn parse_type(s: &str) -> Result<DecisionType> {
    DecisionType::parse(s).ok_or_else(|| {
        anyhow!(
            "unknown --type '{s}'. Valid: {}",
            DecisionType::ALL.map(DecisionType::as_str).join(", ")
        )
    })
}

/// Compact, human description of an effects JSON blob (the projection receipt).
fn effects_summary(effects: &serde_json::Value) -> String {
    // keep_active: explicitly answered, nothing projected — say so calmly
    // instead of leaking the raw effects JSON.
    if effects
        .get("classification")
        .map(serde_json::Value::is_null)
        == Some(true)
    {
        return "kept active — will ask again if it stays untouched".to_owned();
    }
    if let Some(cat) = effects.get("classification").and_then(|v| v.as_str()) {
        if cat == "archive" && effects.get("weight").is_some() {
            // The down-weight is the consequential part — confirm it at the
            // moment of action, not only in the question detail.
            return "archived — kept indexed and searchable, down-weighted in search (reversible)"
                .to_owned();
        }
        return if cat == "ignore" {
            "classification suggestions for this folder are off".to_owned()
        } else {
            format!("folder classified as {cat}")
        };
    }
    if let Some(s) = effects.get("summary") {
        return match s.as_str() {
            Some("restored") => "previous summary restored — its embedding is cleared until \
the path is next regenerated"
                .to_owned(),
            Some(_) => "kept the new summary".to_owned(),
            None => "summary row no longer exists — nothing to restore".to_owned(),
        };
    }
    if let Some(l) = effects.get("language") {
        return match l.as_str() {
            Some(lang) => {
                let n = effects.get("chunks").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("{n} chunk(s) tagged as {lang}")
            }
            None => "left untagged".to_owned(),
        };
    }
    if let Some(a) = effects.get("authoritative") {
        return match a.as_str() {
            Some(p) => format!("{p} recorded as the authoritative definition"),
            None => "all definitions kept equally valid".to_owned(),
        };
    }
    if let Some(canonical) = effects.get("canonical") {
        return match canonical.as_str() {
            Some(c) => {
                let n = effects
                    .get("silenced")
                    .and_then(|v| v.as_array())
                    .map_or(0, Vec::len);
                let copies = if n == 1 {
                    "1 copy".to_owned()
                } else {
                    format!("{n} copies")
                };
                format!("{c} is canonical; {copies} silenced in search (weight 0, reversible)")
            }
            None => "all copies kept searchable".to_owned(),
        };
    }
    effects.to_string()
}

/// One revision-chain line: timestamp, status marker, id, type, outcome —
/// compact enough to scan a chain top to bottom.
fn history_line(d: &DecisionRecord, sgr: &dyn Fn(&str, &str) -> String) -> String {
    let (marker, code) = match d.status.as_str() {
        "open" => ("?", "33"),    // yellow: waiting on the user
        "decided" => ("✓", "32"), // green
        "dismissed" => ("×", "90"),
        "expired" => ("–", "90"),
        _ => ("·", "0"),
    };
    let when = format_unix_timestamp(d.decided_at.unwrap_or(d.created_at));
    let outcome = match (&d.chosen, d.status.as_str()) {
        (Some(c), _) => format!("{c} ({})", d.source.as_deref().unwrap_or("?")),
        (None, "open") => "awaiting answer".to_owned(),
        (None, _) => String::new(),
    };
    let mut line = format!(
        "{}  {} {:<9}  {} {}  {outcome}",
        sgr("2", &when),
        sgr(code, marker),
        d.status,
        sgr("1", &format!("#{}", d.id)),
        sgr("2", &format!("[{}]", d.decision_type)),
    );
    if let Some(n) = d.superseded_by {
        line.push_str(&sgr("2", &format!(" → superseded by #{n}")));
    }
    line
}

fn history_for_subject(store: &Store, subject: &str) -> Result<Vec<DecisionRecord>> {
    let mut rows = Vec::new();
    for t in DecisionType::ALL {
        rows.extend(store.decision_history(t.as_str(), subject)?);
    }
    Ok(rows)
}
