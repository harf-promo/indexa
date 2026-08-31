use anyhow::{bail, Context, Result};
use indexa_core::config::{Config, HybridMode};
use indexa_core::store::Store;
use indexa_query::{
    aggregate, answer_with_ann_history, compare_to_baseline, evaluate_question,
    evaluate_question_reranked, judge_answer, EvalQuestion, EvalSummary, GoldenSet, QaConfig,
    QuestionMetrics,
};
use serde::{Deserialize, Serialize};

use super::helpers::{build_embedder, build_llm, preflight_ollama, require_index_db};

#[derive(Serialize)]
struct EvalJson<'a> {
    mode: &'a str,
    questions: &'a [QuestionMetrics],
    summary: &'a EvalSummary,
}

/// A regression baseline on disk: accepts a full `indexa eval --json` object (extra fields like
/// `mode`/`questions` are ignored) or a bare summary object.
#[derive(Deserialize)]
struct BaselineFile {
    summary: EvalSummary,
}

/// `indexa eval <golden.json>` — regression-test retrieval quality against golden
/// questions. Scores the `retrieve()` ranking the ask pipeline uses, in whichever
/// `--mode` is given: no LLM synthesis, ever; no rerank unless `--rerank` opts in
/// (retrieve() itself never reranks — the LLM/cross-encoder pass runs only afterward,
/// in the real `ask` pipeline); and in sparse mode (the default) no embedder — so a
/// plain run is hermetic and needs no Ollama. This is NOT the full production `ask`
/// ranking unless both `--mode rrf`/`dense` (for MMR) and `--rerank` are set.
/// Exits 1 when the aggregate hit rate falls below `--min-hit-rate`, or (with `--baseline`)
/// when any aggregate metric regresses by more than `--max-regression`.
///
/// `--judge` (opt-in, NOT hermetic) additionally runs real synthesis per question — the same
/// `qa::answer_with_ann_history` entry point `ask` uses, respecting `--rerank` — and grades the
/// synthesized answer with a judge LLM call ([`judge_answer`]). A per-question synthesis/judge
/// failure is logged to stderr and leaves that question's `judge` field `None`; it never aborts
/// the run. `--min-judge-score` can additionally fail the run on a low mean judge score.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_eval(
    golden: String,
    mode: String,
    top_k: usize,
    scope: Option<String>,
    json: bool,
    min_hit_rate: f64,
    baseline: Option<String>,
    max_regression: f64,
    rerank: bool,
    judge: bool,
    judge_model: Option<String>,
    min_judge_score: Option<f64>,
    save_run: Option<String>,
    cfg: &Config,
) -> Result<()> {
    if !(0.0..=1.0).contains(&min_hit_rate) {
        bail!("--min-hit-rate must be between 0.0 and 1.0 (got {min_hit_rate})");
    }
    if max_regression < 0.0 {
        bail!("--max-regression must be >= 0.0 (got {max_regression})");
    }
    if let Some(min_judge) = min_judge_score {
        if !(0.0..=5.0).contains(&min_judge) {
            bail!("--min-judge-score must be between 0.0 and 5.0 (got {min_judge})");
        }
    }
    let hybrid_mode = match mode.as_str() {
        "sparse" => HybridMode::Sparse,
        "rrf" => HybridMode::Rrf,
        "dense" => HybridMode::Dense,
        other => bail!("unknown --mode '{other}'. Valid values: sparse, rrf, dense"),
    };

    let golden_path = shellexpand::tilde(&golden).into_owned();
    let raw = std::fs::read_to_string(&golden_path)
        .with_context(|| format!("cannot read golden file {golden_path}"))?;
    let set: GoldenSet = serde_json::from_str(&raw).with_context(|| {
        format!(
            "cannot parse {golden_path} — expected \
             {{\"questions\": [{{\"question\": .., \"expect_paths\": [..], \"k\"?: ..}}]}}"
        )
    })?;
    if set.questions.is_empty() {
        bail!("golden file {golden_path} has no questions");
    }

    // A gate that can't measure must fail, not silently pass — so missing index /
    // empty index are hard errors (exit 1), unlike the soft hints other commands print.
    let Some(db_path) = require_index_db()? else {
        bail!("eval needs an index");
    };
    let store = Store::open(&db_path)?;
    if store.chunk_count()? == 0 {
        bail!("no deep-scanned content in the index — run `indexa deep <path>` first");
    }

    // Tilde-expand the scope and the expected paths so a golden file can be written
    // portably against $HOME (stored entry paths are absolute).
    let scope = scope.as_deref().map(|s| shellexpand::tilde(s).into_owned());
    let mut questions = set.questions;
    for q in &mut questions {
        for p in &mut q.expect_paths {
            *p = shellexpand::tilde(p.as_str()).into_owned();
        }
    }

    let qa_cfg = QaConfig {
        top_k,
        mode: hybrid_mode,
        scope,
        rrf_k: cfg.retrieval.rrf_k as f32,
        summary_weight: cfg.retrieval.summary_weight,
        summary_depth_alpha: cfg.retrieval.summary_depth_alpha,
        // Off by default so eval stays hermetic; `--rerank` opts into the same rerank pass
        // `ask` uses (see `evaluate_question_reranked` below) — needs a local LLM.
        rerank,
        rerank_backend: cfg.retrieval.rerank_backend.clone(),
        rerank_model: cfg.retrieval.rerank_model.clone(),
        use_weights: cfg.retrieval.use_weights,
        ..QaConfig::default()
    };

    // `--rerank`/`--judge` need a reachable local model exactly like `ask` does. A gate that
    // can't measure must fail, not silently pass (see the index checks above) — so when Ollama
    // is the configured provider and it's unreachable, this is a hard error rather than letting
    // `apply_rerank`'s fail-open behavior quietly score an un-reranked run as reranked (or a
    // `--judge` run silently produce zero verdicts). Note this only checks the DESCRIBER's
    // configured model is pulled — a `--judge-model` naming a different model isn't covered,
    // same limitation `--rerank`'s cross-encoder backend already has.
    if rerank || judge {
        preflight_ollama(cfg).await?;
    }
    let llm = if rerank {
        Some(build_llm(cfg, None)?)
    } else {
        None
    };

    // `--judge` synthesizes a real answer per question (the exact `ask` entry point,
    // `answer_with_ann_history`) before grading it, so it needs its own LLM + embedder — built
    // once, reused across questions. `judge_llm` defaults to the same model as synthesis when
    // `--judge-model` is unset (one model both answers and grades); it's a separate instance so
    // `--judge-model` can point grading at a stronger/cheaper model without touching synthesis.
    let (synth_llm, judge_llm, judge_embedder) = if judge {
        (
            Some(build_llm(cfg, None)?),
            Some(build_llm(cfg, judge_model.as_deref())?),
            Some(build_embedder(cfg, None)?),
        )
    } else {
        (None, None, None)
    };

    // Embed every question up front (rrf/dense only) so the retrieval loop below is
    // fully synchronous — same embed-then-retrieve split as the ask pipeline.
    let query_vecs: Vec<Option<Vec<f32>>> = if matches!(qa_cfg.mode, HybridMode::Sparse) {
        vec![None; questions.len()]
    } else {
        let embedder = build_embedder(cfg, None)?;
        let mut vecs = Vec::with_capacity(questions.len());
        for q in &questions {
            vecs.push(Some(embedder.embed(&q.question).await?));
        }
        vecs
    };

    let mut per_question = Vec::with_capacity(questions.len());
    for (q, vec) in questions.iter().zip(&query_vecs) {
        let mut metrics = match &llm {
            Some(llm) => {
                evaluate_question_reranked(&store, q, &qa_cfg, vec.as_deref(), llm.as_ref()).await?
            }
            None => evaluate_question(&store, q, &qa_cfg, vec.as_deref())?,
        };
        if judge {
            metrics.judge = run_judge_for_question(
                &db_path,
                judge_embedder.as_ref().unwrap().as_ref(),
                synth_llm.as_ref().unwrap().as_ref(),
                judge_llm.as_ref().unwrap().as_ref(),
                q,
                &qa_cfg,
            )
            .await;
        }
        per_question.push(metrics);
    }
    let summary = aggregate(&per_question);
    let eval_json = EvalJson {
        mode: &mode,
        questions: &per_question,
        summary: &summary,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&eval_json)?);
    } else {
        println!(
            "{:>3}  {:>4}  {:>6}  {:>5}  {:>5}  {:>5}  question",
            "hit", "rank", "rr", "prec", "rec", "ndcg"
        );
        for m in &per_question {
            println!(
                "{:>3}  {:>4}  {:>6.3}  {:>5.2}  {:>5.2}  {:>5.2}  {}",
                if m.hit { "✓" } else { "✗" },
                m.first_hit_rank
                    .map_or_else(|| "-".to_owned(), |r| r.to_string()),
                m.reciprocal_rank,
                m.precision,
                m.recall,
                m.ndcg,
                truncate(&m.question, 60),
            );
            // Judge line, printed only when `--judge` produced a verdict for this question —
            // the ranking table above stays byte-identical to a plain (non-`--judge`) run.
            if let Some(j) = &m.judge {
                println!("      judge {}/5 — {}", j.score, j.reason);
            }
        }
        println!();
        // `judge_line` is empty in the default (non-`--judge`) case, so this summary line stays
        // byte-identical to a plain run's output.
        let judge_line = match summary.mean_judge_score {
            Some(mean) => format!(
                " · judge {:.2}/5 ({}/{} graded)",
                mean,
                summary.judged_questions.unwrap_or(0),
                summary.questions
            ),
            None if judge => " · judge: no verdicts (every synthesis/judge call failed)".to_owned(),
            None => String::new(),
        };
        println!(
            "{} questions · hit rate {:.2} · MRR {:.3} · recall {:.2} · nDCG {:.3} · precision {:.2} · mode {}{}",
            summary.questions,
            summary.hit_rate,
            summary.mrr,
            summary.mean_recall,
            summary.mean_ndcg,
            summary.mean_precision,
            mode,
            judge_line
        );
    }

    // Optional `--save-run`: a first-class alternative to manually redirecting `--json` output
    // (see the `--baseline` doc comment) — writes the same payload to a dated file, ready to hand
    // back in later via `--baseline`. Standalone (doesn't require `--json`) and composes with
    // `--baseline`/`--rerank`/`--judge` since it just reuses `eval_json`, already built above.
    // The confirmation goes to stderr when `--json` is set so stdout stays machine-parseable.
    if let Some(dir) = &save_run {
        let path = save_eval_run(dir, &eval_json)?;
        if json {
            eprintln!("Saved run to {path}");
        } else {
            println!("Saved run to {path}");
        }
    }

    // Optional baseline regression gate: load a saved run, print the per-metric deltas, and flag
    // any aggregate that dropped by more than --max-regression.
    let mut regressed = false;
    if let Some(baseline_path) = &baseline {
        let bpath = shellexpand::tilde(baseline_path).into_owned();
        let braw = std::fs::read_to_string(&bpath)
            .with_context(|| format!("cannot read baseline file {bpath}"))?;
        // Accept a full `--json` object (with a `summary` field) or a bare summary object.
        let base: EvalSummary = serde_json::from_str::<BaselineFile>(&braw)
            .map(|b| b.summary)
            .or_else(|_| serde_json::from_str::<EvalSummary>(&braw))
            .with_context(|| {
                format!(
                    "cannot parse baseline {bpath} — expected an `indexa eval --json` output or a summary object"
                )
            })?;
        let deltas = compare_to_baseline(&summary, &base, max_regression);
        regressed = deltas.iter().any(|d| d.regressed);
        if !json {
            let line = deltas
                .iter()
                .map(|d| format!("{} {:+.3}", d.name, d.delta))
                .collect::<Vec<_>>()
                .join(" · ");
            println!("vs baseline: {line}");
        }
        // stderr so --json stdout stays machine-parseable.
        for d in deltas.iter().filter(|d| d.regressed) {
            eprintln!(
                "eval: {} regressed {:.3} → {:.3} (Δ{:+.3}, max allowed -{:.3})",
                d.name, d.baseline, d.current, d.delta, max_regression
            );
        }
    }

    let mut fail = false;
    if summary.hit_rate < min_hit_rate {
        eprintln!(
            "eval: hit rate {:.2} below --min-hit-rate {min_hit_rate:.2}",
            summary.hit_rate
        );
        fail = true;
    }
    if regressed {
        fail = true;
    }
    // A gate that can't measure must fail, not silently pass (same posture as the index checks
    // above): if every synthesis/judge call in the run failed, `mean_judge_score` is `None` —
    // that counts as failing `--min-judge-score`, not as passing it by default.
    if let Some(min_judge) = min_judge_score {
        match summary.mean_judge_score {
            Some(mean) if mean < min_judge => {
                eprintln!(
                    "eval: mean judge score {mean:.2} below --min-judge-score {min_judge:.2}"
                );
                fail = true;
            }
            None => {
                eprintln!(
                    "eval: --min-judge-score set but no question got a judge verdict \
                     (every synthesis/judge call failed)"
                );
                fail = true;
            }
            Some(_) => {}
        }
    }
    if fail {
        std::process::exit(1);
    }
    Ok(())
}

/// Grade one question end to end for `--judge`: run real synthesis (the same
/// `answer_with_ann_history` entry point `ask` uses, respecting `qa_cfg.rerank`), then grade the
/// answer with the judge LLM. Fails open — any synthesis or judge error is logged to stderr and
/// returns `None`, so one bad question never aborts the whole `--judge` run.
async fn run_judge_for_question(
    db_path: &std::path::Path,
    embedder: &dyn indexa_embed::Embedder,
    synth_llm: &dyn indexa_llm::Generator,
    judge_llm: &dyn indexa_llm::Generator,
    q: &EvalQuestion,
    qa_cfg: &QaConfig,
) -> Option<indexa_query::JudgeVerdict> {
    let answer =
        match answer_with_ann_history(db_path, embedder, synth_llm, &q.question, qa_cfg, None, &[])
            .await
        {
            Ok(a) => a,
            Err(e) => {
                eprintln!("eval: --judge synthesis failed for {:?}: {e:#}", q.question);
                return None;
            }
        };
    match judge_answer(
        judge_llm,
        &q.question,
        &answer.sources,
        &answer.answer,
        q.expect_answer_hint.as_deref(),
    )
    .await
    {
        Ok(verdict) => Some(verdict),
        Err(e) => {
            eprintln!("eval: --judge grading failed for {:?}: {e:#}", q.question);
            None
        }
    }
}

/// `--save-run`: write `json` (the same shape `--json` prints) to `<dir>/eval-<unix>.json` and
/// return the path written. `dir` is tilde-expanded and created if missing. The filename keys on
/// `helpers::now_unix()` so repeated runs never collide and sort chronologically.
fn save_eval_run(dir: &str, payload: &EvalJson<'_>) -> Result<String> {
    let dir = super::helpers::expand(dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating --save-run directory '{dir}'"))?;
    let path = std::path::Path::new(&dir)
        .join(format!("eval-{}.json", super::helpers::now_unix()))
        .to_string_lossy()
        .into_owned();
    let body = serde_json::to_string_pretty(payload)?;
    std::fs::write(&path, body)
        .with_context(|| format!("writing --save-run output to '{path}'"))?;
    Ok(path)
}

/// Char-safe truncation for the table's question column.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::{save_eval_run, truncate, EvalJson};
    use indexa_query::{EvalSummary, GoldenSet, QuestionMetrics};

    #[test]
    fn golden_file_parses_with_and_without_k() {
        let set: GoldenSet = serde_json::from_str(
            r#"{"questions": [
                {"question": "where is auth handled?", "expect_paths": ["/repo/src/auth.rs"]},
                {"question": "db pooling?", "expect_paths": ["/repo/src/db.rs"], "k": 5}
            ]}"#,
        )
        .unwrap();
        assert_eq!(set.questions.len(), 2);
        assert_eq!(set.questions[0].k, None);
        assert_eq!(set.questions[1].k, Some(5));
        assert_eq!(set.questions[1].expect_paths, vec!["/repo/src/db.rs"]);
    }

    #[test]
    fn golden_file_missing_expect_paths_is_an_error() {
        let res: Result<GoldenSet, _> =
            serde_json::from_str(r#"{"questions": [{"question": "q"}]}"#);
        assert!(res.is_err());
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        assert_eq!(truncate("short", 60), "short");
        // Multibyte content must not panic and must end with the ellipsis.
        let long = "é".repeat(80);
        let cut = truncate(&long, 10);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), 10);
    }

    fn sample_metrics() -> Vec<QuestionMetrics> {
        vec![QuestionMetrics {
            question: "where is auth handled?".to_owned(),
            k: 10,
            retrieved: 3,
            hit: true,
            first_hit_rank: Some(1),
            reciprocal_rank: 1.0,
            precision: 0.5,
            recall: 1.0,
            ndcg: 1.0,
            judge: None,
        }]
    }

    fn sample_summary() -> EvalSummary {
        EvalSummary {
            questions: 1,
            hit_rate: 1.0,
            mrr: 1.0,
            mean_precision: 0.5,
            mean_recall: 1.0,
            mean_ndcg: 1.0,
            mean_judge_score: None,
            judged_questions: None,
        }
    }

    /// `--save-run` writes `<dir>/eval-<unix>.json` with the exact `{mode, questions, summary}`
    /// shape `--json` prints — this is what a later `--baseline` load expects to parse back.
    #[test]
    fn save_eval_run_writes_dated_file_with_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let questions = sample_metrics();
        let summary = sample_summary();
        let eval_json = EvalJson {
            mode: "sparse",
            questions: &questions,
            summary: &summary,
        };

        let path = save_eval_run(dir.path().to_str().unwrap(), &eval_json).unwrap();

        assert!(
            path.starts_with(dir.path().to_str().unwrap()),
            "path {path} must live under the requested --save-run dir"
        );
        let filename = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            filename.starts_with("eval-") && filename.ends_with(".json"),
            "got: {filename}"
        );
        let unix_part = filename
            .strip_prefix("eval-")
            .and_then(|s| s.strip_suffix(".json"))
            .unwrap();
        assert!(
            unix_part.parse::<i64>().is_ok(),
            "filename's timestamp segment must be a plain unix seconds integer, got: {unix_part}"
        );

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["mode"], "sparse");
        assert_eq!(parsed["summary"]["questions"], 1);
        assert_eq!(parsed["questions"][0]["question"], "where is auth handled?");
    }

    /// A nonexistent `--save-run` directory is created, not an error — mirrors `finalize_export`
    /// only erroring on a *file* write into a missing parent, never on the `--save-run` dir itself.
    #[test]
    fn save_eval_run_creates_missing_directory() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("does/not/exist/yet");
        let questions = sample_metrics();
        let summary = sample_summary();
        let eval_json = EvalJson {
            mode: "sparse",
            questions: &questions,
            summary: &summary,
        };

        let path = save_eval_run(nested.to_str().unwrap(), &eval_json).unwrap();
        assert!(std::path::Path::new(&path).exists());
    }
}
