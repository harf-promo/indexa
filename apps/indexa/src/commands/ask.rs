use anyhow::Result;
use indexa_core::{config::HybridMode, store::Store};
use indexa_query::{
    answer_agentic_history, answer_with_ann_history, explain_retrieval, served_bytes, Answer,
    AnswerImpact, Confidence, PriorTurn, QaConfig, RetrievalTrace,
};
use serde::Serialize;

use super::helpers::{build_embedder, build_llm, require_index_db};
use indexa_core::config::Config;

// ── JSON output DTOs (the query types aren't Serialize; map to a stable shape here) ──

#[derive(Serialize)]
struct SourceJson {
    path: String,
    heading: String,
    snippet: String,
}

#[derive(Serialize)]
struct HitJson {
    path: String,
    heading: String,
    seq: usize,
    score: f64,
}

#[derive(Serialize)]
struct StageJson {
    label: String,
    hits: Vec<HitJson>,
}

#[derive(Serialize)]
struct RetrievalJson {
    mode: String,
    top_k: usize,
    rrf_k: f32,
    rerank: bool,
    use_weights: bool,
    scope: Option<String>,
    stages: Vec<StageJson>,
}

#[derive(Serialize)]
struct ConfidenceJson {
    level: &'static str,
    basis: String,
    /// Salient question terms absent from every cited source (heuristic coverage gap).
    #[serde(skip_serializing_if = "Option::is_none")]
    uncovered: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ImpactJson {
    served_bytes: u64,
    counterfactual_bytes: u64,
    saved_percent: u8,
}

#[derive(Serialize)]
struct AnswerJson {
    question: String,
    answer: String,
    sources: Vec<SourceJson>,
    /// Retrieval-shape confidence; absent for the no-match short-circuit.
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<ConfidenceJson>,
    /// Per-answer byte savings vs. the cited files whole; absent when nothing to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    impact: Option<ImpactJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retrieval: Option<RetrievalJson>,
    /// Conversation id this turn was recorded under (Conversational Ask); absent when stateless.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

fn trace_to_json(trace: &RetrievalTrace) -> RetrievalJson {
    RetrievalJson {
        mode: trace.mode.clone(),
        top_k: trace.top_k,
        rrf_k: trace.rrf_k,
        rerank: trace.rerank,
        use_weights: trace.use_weights,
        scope: trace.scope.clone(),
        stages: trace
            .stages
            .iter()
            .map(|s| StageJson {
                label: s.label.clone(),
                hits: s
                    .hits
                    .iter()
                    .map(|h| HitJson {
                        path: h.entry_path.clone(),
                        heading: h.heading.clone(),
                        seq: h.seq,
                        score: h.rrf_score,
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Print a human-readable retrieval trace (the `--explain` view).
fn print_trace(trace: &RetrievalTrace) {
    println!(
        "Retrieval trace  (mode={}, top_k={}, rrf_k={:.0}, rerank={}, weights={})",
        trace.mode,
        trace.top_k,
        trace.rrf_k,
        if trace.rerank { "on" } else { "off" },
        if trace.use_weights { "on" } else { "off" },
    );
    println!("  scope: {}", trace.scope.as_deref().unwrap_or("<none>"));
    for stage in &trace.stages {
        println!();
        println!("  ▸ {} — {} hit(s)", stage.label, stage.hits.len());
        for (i, h) in stage.hits.iter().enumerate() {
            let loc = if h.heading.is_empty() {
                h.entry_path.clone()
            } else {
                format!("{} — {}", h.entry_path, h.heading)
            };
            println!("     {:>2}. [{:.4}] {}", i + 1, h.rrf_score, loc);
        }
    }
    println!();
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_ask(
    question: String,
    embed_model_flag: Option<String>,
    llm_model_flag: Option<String>,
    scope_flag: Option<String>,
    top_k_flag: Option<usize>,
    sparse_only: bool,
    dense_only: bool,
    agentic_flag: bool,
    max_steps_flag: Option<usize>,
    explain: bool,
    session_id_flag: Option<String>,
    continue_: bool,
    json: bool,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = Store::open(&db_path)?;
    let chunk_count = store.chunk_count()?;
    if chunk_count == 0 {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&AnswerJson {
                    question,
                    answer: "No deep-scanned content found. Run `indexa index <path>` first."
                        .to_owned(),
                    sources: Vec::new(),
                    confidence: None,
                    impact: None,
                    retrieval: None,
                    session_id: None,
                })?
            );
        } else {
            println!("No deep-scanned content found. Run `indexa index <path>` first.");
        }
        return Ok(());
    }

    let embedder = build_embedder(cfg, embed_model_flag.as_deref())?;
    let llm = build_llm(cfg, llm_model_flag.as_deref())?;

    let mode = if sparse_only {
        HybridMode::Sparse
    } else if dense_only {
        HybridMode::Dense
    } else {
        cfg.retrieval.hybrid.clone()
    };

    let scope = scope_flag
        .as_deref()
        .map(|s| shellexpand::tilde(s).into_owned());

    // --max-steps implies --agentic; otherwise fall back to the config default.
    // (clap guarantees --explain is never combined with --agentic/--max-steps.)
    let agentic = agentic_flag || max_steps_flag.is_some() || cfg.retrieval.agentic;
    let max_steps = max_steps_flag.unwrap_or(cfg.retrieval.agentic_max_steps);

    let qa_cfg = QaConfig {
        top_k: top_k_flag.unwrap_or(cfg.retrieval.top_k),
        mode,
        scope,
        context_budget: cfg.retrieval.context_budget,
        rrf_k: cfg.retrieval.rrf_k as f32,
        summary_weight: cfg.retrieval.summary_weight,
        summary_depth_alpha: cfg.retrieval.summary_depth_alpha,
        rerank: cfg.retrieval.rerank,
        rerank_backend: cfg.retrieval.rerank_backend.clone(),
        use_weights: cfg.retrieval.use_weights,
        use_recency_weight: cfg.retrieval.recency_boost,
        recency_days: cfg.retrieval.recency_days,
        max_steps,
        mmr_lambda: cfg.retrieval.mmr_lambda,
    };

    // `store` is no longer needed by the query path — the pipeline opens its own
    // scoped connection. Drop it so we don't hold two handles open.
    drop(store);

    // Conversational Ask: resolve the effective session id (--session-id wins; --continue
    // reuses the last one) and load its recent turns. Explain mode never threads a session
    // (clap forbids --continue with --explain; an explicit --session-id is simply ignored
    // for the answer-free trace). Empty history ⇒ a stateless ask (today's default).
    let session_id = session_id_flag.or_else(|| if continue_ { read_last_session() } else { None });
    let history = if explain {
        Vec::new()
    } else {
        load_cli_history(&db_path, session_id.as_deref(), qa_cfg.scope.as_deref())
    };

    // --explain: build the retrieval trace first (one-shot path; clap forbids agentic here).
    let trace = if explain {
        let t = explain_retrieval(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
            None,
        )
        .await?;
        if !json {
            print_trace(&t);
        }
        Some(t)
    } else {
        None
    };

    let answer = if agentic {
        if !json {
            println!(
                "Searching {chunk_count} indexed chunks (agentic, up to {max_steps} hops)...\n"
            );
        }
        let mut on_step = |step: usize, query: &str| {
            if !json {
                println!("  🔍 step {step}: {query}");
            }
        };
        let ans = answer_agentic_history(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
            &history,
            &mut on_step,
        )
        .await?;
        if !json {
            println!();
        }
        ans
    } else {
        if !json && !explain {
            println!("Searching {chunk_count} indexed chunks...\n");
        }
        answer_with_ann_history(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
            None,
            &history,
        )
        .await?
    };

    // Best-effort token-savings telemetry + the per-answer impact readout — must never fail
    // the user's ask. (`store` was dropped above so the query path didn't hold two handles; a
    // fresh open here is the same cost every other command pays.)
    let impact: Option<AnswerImpact> = match Store::open(&db_path) {
        Ok(mut s) => {
            let paths: Vec<&str> = answer.sources.iter().map(|x| x.path.as_str()).collect();
            let counterfactual = s.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
            // Served = answer + delivered citations (shared `served_bytes`, consistent with the
            // web surface). v0.59: this replaced the old answer-text-only count, which slightly
            // undercounted served bytes (and so overstated savings) in the aggregate `status`.
            let served = served_bytes(&answer);
            if let Err(e) = s.record_tool_usage("cli", "ask", served, counterfactual) {
                tracing::debug!("usage telemetry skipped: {e:#}");
            }
            Some(AnswerImpact::new(served, counterfactual))
        }
        Err(e) => {
            tracing::debug!("usage telemetry skipped: {e:#}");
            None
        }
    };
    // Only surface the readout when it's a real win (cited files existed and serving was
    // smaller) — never a misleading "0% saved" on a no-match answer.
    let impact = impact.filter(AnswerImpact::is_meaningful);

    // Conversational Ask: persist this turn (best-effort) and remember it as the latest
    // conversation so a later `--continue` resumes it.
    if let Some(id) = session_id.as_deref() {
        append_cli_turn(&db_path, id, &question, &answer);
        write_last_session(id);
    }

    if json {
        let out = AnswerJson {
            question: answer.question.clone(),
            answer: answer.answer.clone(),
            sources: answer
                .sources
                .iter()
                .map(|s| SourceJson {
                    path: s.path.clone(),
                    heading: s.heading.clone(),
                    snippet: s.snippet.clone(),
                })
                .collect(),
            confidence: answer.confidence.as_ref().map(|c| ConfidenceJson {
                level: c.level.as_str(),
                basis: c.basis.clone(),
                uncovered: c.uncovered.clone(),
            }),
            impact: impact.map(|i| ImpactJson {
                served_bytes: i.served_bytes,
                counterfactual_bytes: i.counterfactual_bytes,
                saved_percent: i.saved_percent(),
            }),
            retrieval: trace.as_ref().map(trace_to_json),
            session_id: session_id.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("Answer:\n{}\n", answer.answer);

    if !answer.sources.is_empty() {
        println!("Sources:");
        for (i, src) in answer.sources.iter().enumerate() {
            let loc = if src.heading.is_empty() {
                src.path.clone()
            } else {
                format!("{} — {}", src.path, src.heading)
            };
            println!("  [{}] {}", i + 1, loc);
        }
    }

    // Retrieval-shape confidence (absent only for the no-match short-circuit —
    // that message already says the index has nothing).
    if let Some(c) = &answer.confidence {
        use std::io::IsTerminal;
        // Below High with no scope set, narrowing the search is the one lever
        // the user can pull right now.
        let hint = if c.level != Confidence::High && qa_cfg.scope.is_none() {
            "; consider scoping with --scope"
        } else {
            ""
        };
        let line = format!("confidence: {} — {}{}", c.level, c.basis, hint);
        if std::io::stdout().is_terminal() {
            println!("\n\x1b[2m{line}\x1b[0m");
        } else {
            println!("\n{line}");
        }
        // Heuristic coverage gap: question terms found in none of the cited sources.
        if let Some(gaps) = c.uncovered.as_ref().filter(|g| !g.is_empty()) {
            let line = format!("  may not cover: {}", gaps.join(", "));
            if std::io::stdout().is_terminal() {
                println!("\x1b[2m{line}\x1b[0m");
            } else {
                println!("{line}");
            }
        }
        // --explain: the raw shape numbers the level was derived from (heuristic,
        // not calibrated — see indexa_query::assess_confidence).
        if explain {
            let i = &c.inputs;
            println!(
                "  inputs: {}/{} hits · top {:.4} · median {:.4} · gap {:.1}× · \
                 {} strong (floor {:.4}) · embeddings {}",
                i.hit_count,
                i.top_k,
                i.top_score,
                i.median_score,
                i.gap,
                i.strong_hits,
                i.strong_floor,
                if i.embeddings { "on" } else { "off" },
            );
        }
    }

    // The "retrieve the slice" win, made concrete for this answer.
    if let Some(i) = impact {
        use std::io::IsTerminal;
        let line = format!("impact: {}", i.human());
        if std::io::stdout().is_terminal() {
            println!("\n\x1b[2m{line}\x1b[0m");
        } else {
            println!("\n{line}");
        }
    }

    // Conversational Ask: show the id so the user can `--continue` (or `--session-id <id>`).
    if let Some(id) = &session_id {
        use std::io::IsTerminal;
        let line = format!("session: {id}  (continue with `indexa ask --continue \"...\"`)");
        if std::io::stdout().is_terminal() {
            println!("\n\x1b[2m{line}\x1b[0m");
        } else {
            println!("\n{line}");
        }
    }

    Ok(())
}

/// Path of the pointer file remembering the most recent conversation id (for `--continue`).
fn last_session_path() -> Option<std::path::PathBuf> {
    indexa_core::config::default_data_dir().map(|d| d.join("last_ask_session"))
}

/// The last conversation id, if any (`--continue`). Best-effort.
fn read_last_session() -> Option<String> {
    let p = last_session_path()?;
    let id = std::fs::read_to_string(p).ok()?.trim().to_owned();
    (!id.is_empty()).then_some(id)
}

/// Remember `id` as the most recent conversation. Best-effort (a write failure is non-fatal).
fn write_last_session(id: &str) {
    if let Some(p) = last_session_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, id);
    }
}

/// Ensure the session exists and load its recent turns as [`PriorTurn`]s. Fail-open: any
/// error ⇒ no history (a stateless ask). `None` session_id ⇒ empty.
fn load_cli_history(
    db_path: &std::path::Path,
    session_id: Option<&str>,
    scope: Option<&str>,
) -> Vec<PriorTurn> {
    const HISTORY_TURNS: usize = 6;
    let Some(id) = session_id else {
        return Vec::new();
    };
    let Ok(mut store) = Store::open(db_path) else {
        return Vec::new();
    };
    if store.ensure_session(id, scope).is_err() {
        return Vec::new();
    }
    store
        .recent_turns(id, HISTORY_TURNS)
        .map(|turns| {
            turns
                .into_iter()
                .map(|t| PriorTurn {
                    question: t.question,
                    answer: t.answer,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Persist a completed turn, best-effort (serializes citations to the opaque `sources_json`).
fn append_cli_turn(db_path: &std::path::Path, session_id: &str, question: &str, answer: &Answer) {
    let sources_json = serde_json::to_string(
        &answer
            .sources
            .iter()
            .map(|s| {
                serde_json::json!({ "path": s.path, "heading": s.heading, "snippet": s.snippet })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned());
    if let Ok(mut store) = Store::open(db_path) {
        let _ = store.append_turn(session_id, question, &answer.answer, &sources_json);
    }
}
