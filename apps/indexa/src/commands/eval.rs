use anyhow::{bail, Context, Result};
use indexa_core::config::{Config, HybridMode};
use indexa_core::store::Store;
use indexa_query::{
    aggregate, evaluate_question, EvalSummary, GoldenSet, QaConfig, QuestionMetrics,
};
use serde::Serialize;

use super::helpers::{build_embedder, require_index_db};

#[derive(Serialize)]
struct EvalJson<'a> {
    mode: &'a str,
    questions: &'a [QuestionMetrics],
    summary: &'a EvalSummary,
}

/// `indexa eval <golden.json>` — regression-test retrieval quality against golden
/// questions. Retrieval only (the same `retrieve()` the ask pipeline uses): no LLM,
/// no rerank, and in sparse mode (the default) no embedder — so a CI run is hermetic.
/// Exits 1 when the aggregate hit rate falls below `--min-hit-rate`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_eval(
    golden: String,
    mode: String,
    top_k: usize,
    scope: Option<String>,
    json: bool,
    min_hit_rate: f64,
    cfg: &Config,
) -> Result<()> {
    if !(0.0..=1.0).contains(&min_hit_rate) {
        bail!("--min-hit-rate must be between 0.0 and 1.0 (got {min_hit_rate})");
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
        rerank: false, // rerank needs an LLM; eval stays hermetic
        use_weights: cfg.retrieval.use_weights,
        ..QaConfig::default()
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
        per_question.push(evaluate_question(&store, q, &qa_cfg, vec.as_deref())?);
    }
    let summary = aggregate(&per_question);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&EvalJson {
                mode: &mode,
                questions: &per_question,
                summary: &summary,
            })?
        );
    } else {
        println!(
            "{:>3}  {:>4}  {:>6}  {:>5}  question",
            "hit", "rank", "rr", "prec"
        );
        for m in &per_question {
            println!(
                "{:>3}  {:>4}  {:>6.3}  {:>5.2}  {}",
                if m.hit { "✓" } else { "✗" },
                m.first_hit_rank
                    .map_or_else(|| "-".to_owned(), |r| r.to_string()),
                m.reciprocal_rank,
                m.precision,
                truncate(&m.question, 60),
            );
        }
        println!();
        println!(
            "{} questions · hit rate {:.2} · MRR {:.3} · precision {:.2} · mode {}",
            summary.questions, summary.hit_rate, summary.mrr, summary.mean_precision, mode
        );
    }

    if summary.hit_rate < min_hit_rate {
        // stderr so --json stdout stays machine-parseable.
        eprintln!(
            "eval: hit rate {:.2} below --min-hit-rate {min_hit_rate:.2}",
            summary.hit_rate
        );
        std::process::exit(1);
    }
    Ok(())
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
    use super::truncate;
    use indexa_query::GoldenSet;

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
}
