use anyhow::Result;
use indexa_core::{config::HybridMode, store::Store};
use indexa_query::{
    answer, answer_agentic, explain_retrieval, Confidence, QaConfig, RetrievalTrace,
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
}

#[derive(Serialize)]
struct AnswerJson {
    question: String,
    answer: String,
    sources: Vec<SourceJson>,
    /// Retrieval-shape confidence; absent for the no-match short-circuit.
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<ConfidenceJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retrieval: Option<RetrievalJson>,
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
                    answer: "No deep-scanned content found. Run `indexa deep <path>` first."
                        .to_owned(),
                    sources: Vec::new(),
                    confidence: None,
                    retrieval: None,
                })?
            );
        } else {
            println!("No deep-scanned content found. Run `indexa deep <path>` first.");
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
        use_weights: cfg.retrieval.use_weights,
        max_steps,
    };

    // `store` is no longer needed by the query path — `answer` opens its own
    // scoped connection. Drop it so we don't hold two handles open.
    drop(store);

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
        let ans = answer_agentic(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
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
        answer(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
        )
        .await?
    };

    // Best-effort token-savings telemetry — must never fail the user's ask.
    // (`store` was dropped above so the query path didn't hold two handles; a
    // fresh open here is the same cost every other command pays.)
    match Store::open(&db_path) {
        Ok(mut s) => {
            let paths: Vec<&str> = answer.sources.iter().map(|x| x.path.as_str()).collect();
            let counterfactual = s.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
            if let Err(e) =
                s.record_tool_usage("cli", "ask", answer.answer.len() as u64, counterfactual)
            {
                tracing::debug!("usage telemetry skipped: {e:#}");
            }
        }
        Err(e) => tracing::debug!("usage telemetry skipped: {e:#}"),
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
            }),
            retrieval: trace.as_ref().map(trace_to_json),
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

    Ok(())
}
