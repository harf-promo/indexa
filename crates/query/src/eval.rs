//! Retrieval-quality evaluation backing `indexa eval`.
//!
//! Scores the `retrieve()` ranking the ask pipeline uses (hybrid search + summary/
//! importance boosts + MMR in non-sparse modes) against a golden-questions file —
//! retrieval only, no LLM synthesis by default. `retrieve()` never applies the
//! cross-encoder/LLM rerank pass itself — that happens afterward, only in the real
//! `ask` pipeline (`qa::synthesize::retrieve_and_rerank`) — so by default this gate
//! cannot detect a reranker regression regardless of mode. [`evaluate_question_reranked`]
//! is the opt-in counterpart (`indexa eval --rerank`) that routes retrieval through the
//! SAME `apply_configured_rerank` dispatch `ask` uses, so a reranker regression becomes
//! visible when a caller asks for it; plain [`evaluate_question`] stays LLM-free and
//! hermetic. In sparse mode (CI's default) `retrieve()` additionally skips MMR entirely
//! (`retrieve()` only applies MMR outside `HybridMode::Sparse`), so a sparse-mode run is
//! deterministic and needs no Ollama, but is scoring strictly less of the ranking
//! pipeline than `rrf`/`dense` mode does. This is the regression gate for
//! retrieval-affecting changes (chunking, parsing, ranking, optionally reranking)
//! within that scope — see `docs/methodology.md` for the A/B recipe that covers
//! dense-mode retrieval.
//!
//! **`--judge` (opt-in, not hermetic)**: everything above scores *ranking* — which chunks
//! came back, and where — but says nothing about the ANSWER text a real `ask` would
//! synthesize from them. [`judge_answer`] grades that: given the question, the sources an
//! actual synthesis call cited, and the synthesized answer text, one judge LLM call scores
//! 0-5 against a fixed rubric (does the answer address the question; is every claim
//! supported by the sources) and returns a one-sentence rationale. This is deliberately a
//! SEPARATE, later-composed concern from ranking scoring — `judge_answer` takes plain
//! `question`/`sources`/`answer` strings, not a `Store` or `QaConfig`, so it has no opinion
//! on how the answer was produced. The CLI (`indexa eval --judge`) is what wires it to a
//! real synthesis call (`qa::answer_with_ann_history`, the same entry point `ask` uses) per
//! question. A judge verdict is purely additive on [`QuestionMetrics`]/[`EvalSummary`] (both
//! `Option`, `#[serde(default)]` on the summary fields) — a plain run's output, and an old
//! saved baseline's deserialization, are unaffected.

use anyhow::{Context, Result};
use indexa_core::store::Store;
use indexa_llm::Generator;
use serde::{Deserialize, Serialize};

use crate::qa::{retrieve, QaConfig, SourceCitation};
use crate::rerank::apply_configured_rerank;

/// One golden question: a query plus the file paths a correct retrieval must surface.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalQuestion {
    pub question: String,
    /// Paths exactly as stored in the index (absolute; the CLI tilde-expands them).
    pub expect_paths: Vec<String>,
    /// Per-question cutoff; falls back to the run-level top-k when unset.
    #[serde(default)]
    pub k: Option<usize>,
    /// Optional human-written note on what a correct answer should mention, e.g. "must name
    /// the RRF fusion step and cite qa.rs". Included in the `--judge` rubric prompt when
    /// present; the judge grades purely on question+sources+answer when absent. Backward
    /// compatible: existing golden files (and `fixtures/self-golden.json`) need no changes.
    #[serde(default)]
    pub expect_answer_hint: Option<String>,
}

/// The golden file root: `{"questions": [...]}`.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenSet {
    pub questions: Vec<EvalQuestion>,
}

/// Scores for one question's ranked hits.
#[derive(Debug, Clone, Serialize)]
pub struct QuestionMetrics {
    pub question: String,
    /// The cutoff this question was scored at.
    pub k: usize,
    /// Hits actually returned (≤ k — a small index can run out of matches).
    pub retrieved: usize,
    /// hit@k: at least one expected path appeared in the top k.
    pub hit: bool,
    /// 1-based rank of the first expected path; `None` on a miss.
    pub first_hit_rank: Option<usize>,
    /// 1/first_hit_rank, 0.0 on a miss. Averaged into the summary MRR.
    pub reciprocal_rank: f64,
    /// Citation precision: fraction of *returned* hits whose entry_path is expected
    /// (denominator is `retrieved`, not `k`, so 3 relevant of 3 returned scores 1.0).
    pub precision: f64,
    /// recall@k: fraction of the *distinct expected paths* covered by some top-k hit
    /// (1.0 when every expected path was retrieved). Complements `hit`/`precision` —
    /// hit@k only asks "any expected path?", recall asks "how many of them?".
    pub recall: f64,
    /// nDCG@k with binary relevance: how well the expected hits are ranked *within*
    /// the top-k, normalized so 1.0 = the relevant hits packed at the top ranks.
    /// Catches rank demotions (expected hit slides from #1 to #6) that hit@k cannot.
    pub ndcg: f64,
    /// LLM-judge verdict on the synthesized answer (`indexa eval --judge`). `None` in default
    /// (ranking-only) mode, and `None` for a question whose synthesis or judge call failed —
    /// a judge failure is a per-question miss, not fatal to the whole run. `skip_serializing_if`
    /// keeps a plain (non-`--judge`) run's `--json` output byte-identical to before this field
    /// existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeVerdict>,
}

/// One LLM-judge verdict: how well a synthesized answer addresses the question and how well
/// every claim in it is supported by the sources it cited. **Not calibrated** — like
/// `assess_confidence`'s retrieval-shape confidence, this is a heuristic single-model judgment,
/// not a probability. See [`judge_answer`].
#[derive(Debug, Clone, Serialize)]
pub struct JudgeVerdict {
    /// 0 (fails the rubric) – 5 (fully addresses the question, every claim supported).
    pub score: u8,
    /// The judge's one-sentence rationale, taken verbatim from its response.
    pub reason: String,
}

/// Aggregate over all questions in a run. `Deserialize` so a saved `--json` run can be
/// loaded back as a regression baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSummary {
    pub questions: usize,
    /// Fraction of questions with at least one expected path in their top k (hit@k).
    pub hit_rate: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
    pub mean_precision: f64,
    /// Mean recall@k across questions.
    pub mean_recall: f64,
    /// Mean nDCG@k across questions.
    pub mean_ndcg: f64,
    /// Mean [`JudgeVerdict::score`] (0-5) across questions that got a verdict
    /// (`indexa eval --judge`). `None` when judge mode wasn't used, or when every judge call in
    /// the run failed. `Option` + `#[serde(default)]` so an OLD saved baseline JSON (written
    /// before this field existed) still deserializes into the current `EvalSummary` — it just
    /// loads as `None`, exactly as a non-`--judge` run would.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_judge_score: Option<f64>,
    /// How many questions actually got a judge verdict (≤ `questions`; a synthesis or judge
    /// parse failure drops a question from this count without failing the run). `None` under
    /// the same conditions as `mean_judge_score`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged_questions: Option<usize>,
}

/// Run retrieval for one golden question and score the ranking. `query_vec` is
/// `None` in sparse mode (mirrors the ask pipeline's embed-skip). Synchronous on
/// purpose: no LLM, no embedder — the caller embeds up front when the mode needs it.
pub fn evaluate_question(
    store: &Store,
    q: &EvalQuestion,
    cfg: &QaConfig,
    query_vec: Option<&[f32]>,
) -> Result<QuestionMetrics> {
    let k = q.k.unwrap_or(cfg.top_k).max(1);
    let mut run_cfg = cfg.clone();
    run_cfg.top_k = k;
    let hits = retrieve(store, &q.question, query_vec, &run_cfg, None)?;
    let ranked: Vec<&str> = hits.iter().map(|h| h.entry_path.as_str()).collect();
    Ok(score_ranking(&q.question, k, &ranked, &q.expect_paths))
}

/// Async counterpart to [`evaluate_question`] for `indexa eval --rerank`: identical retrieval,
/// then — when `cfg.rerank` is set — the SAME `apply_configured_rerank` dispatch the real `ask`
/// pipeline runs (`qa::synthesize::retrieve_and_rerank`, `qa::explain::explain_retrieval`), not a
/// parallel reimplementation. `llm` backs the `"llm"` rerank_backend (the default); the
/// `"cross-encoder"` backend ignores it. When `cfg.rerank` is `false` this scores identically to
/// [`evaluate_question`] — the flag is purely additive.
pub async fn evaluate_question_reranked(
    store: &Store,
    q: &EvalQuestion,
    cfg: &QaConfig,
    query_vec: Option<&[f32]>,
    llm: &dyn Generator,
) -> Result<QuestionMetrics> {
    let k = q.k.unwrap_or(cfg.top_k).max(1);
    let mut run_cfg = cfg.clone();
    run_cfg.top_k = k;
    let hits = retrieve(store, &q.question, query_vec, &run_cfg, None)?;
    let hits = if run_cfg.rerank {
        apply_configured_rerank(llm, &run_cfg, &q.question, hits).await
    } else {
        hits
    };
    let ranked: Vec<&str> = hits.iter().map(|h| h.entry_path.as_str()).collect();
    Ok(score_ranking(&q.question, k, &ranked, &q.expect_paths))
}

/// Build the rubric prompt sent to the judge LLM for one synthesized answer. Split out from
/// [`judge_answer`] so the prompt shape is testable without a fake `Generator`. `hint` is
/// `EvalQuestion::expect_answer_hint`, included only when present (see the module doc).
fn build_judge_prompt(
    question: &str,
    sources: &[SourceCitation],
    answer: &str,
    hint: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are grading an AI assistant's answer against the sources it was given. Score it \
         strictly on two things:\n\
         1. Does the answer directly address the question?\n\
         2. Is every factual claim in the answer supported by the sources below — no invented \
         facts, no unsupported claims?\n\n\
         Respond in EXACTLY this two-line format, nothing else:\n\
         SCORE: <integer 0-5>\n\
         REASON: <one sentence>\n\n",
    );
    prompt.push_str("Question: ");
    prompt.push_str(question);
    prompt.push('\n');
    if let Some(h) = hint {
        prompt.push_str("A correct answer should mention: ");
        prompt.push_str(h);
        prompt.push('\n');
    }
    prompt.push_str("\nSources:\n");
    if sources.is_empty() {
        prompt.push_str("(none retrieved)\n");
    }
    for (i, s) in sources.iter().enumerate() {
        let loc = if s.heading.is_empty() {
            s.path.clone()
        } else {
            format!("{} — {}", s.path, s.heading)
        };
        prompt.push_str(&format!("[{}] {}\n{}\n\n", i + 1, loc, s.snippet));
    }
    prompt.push_str("Answer to grade:\n");
    prompt.push_str(answer);
    prompt.push('\n');
    prompt
}

/// Parse the judge LLM's response into a [`JudgeVerdict`]. Tolerant of a `Score:`/`score:`
/// casing mismatch and a stray preamble line (models don't always obey "nothing else" — same
/// fail-open posture as [`crate::rerank::LlmReranker`]'s ranking parse); clamps an
/// out-of-rubric score (a model that says "6" or "-1") into `0..=5` rather than failing the
/// whole question over it. Errors only when no `SCORE:` line parses at all.
fn parse_judge_response(raw: &str) -> Result<JudgeVerdict> {
    let mut score: Option<u8> = None;
    let mut reason = String::new();
    for line in raw.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("score:") {
            // Anywhere-in-line, matching REASON: below — a model that prefaces the score with
            // "Final score: 4" or similar must parse the same as a bare "SCORE: 4" line.
            let n_str = line[idx + "score:".len()..]
                .trim()
                .trim_end_matches(['.', '/']);
            if let Ok(n) = n_str.trim().parse::<i64>() {
                score = Some(n.clamp(0, 5) as u8);
            }
        } else if let Some(idx) = lower.find("reason:") {
            reason = line[idx + "reason:".len()..].trim().to_owned();
        }
    }
    let score =
        score.ok_or_else(|| anyhow::anyhow!("judge response had no parseable SCORE: line"))?;
    if reason.is_empty() {
        reason = "(no reason given)".to_owned();
    }
    Ok(JudgeVerdict { score, reason })
}

/// Grade one synthesized answer against the rubric via a single judge-LLM call. Pure w.r.t.
/// how the answer was produced — takes plain strings (question/sources/answer), not a `Store`
/// or `QaConfig`, so it composes with whatever synthesis path a caller used (the CLI wires it
/// to `qa::answer_with_ann_history`, the same entry point `ask` uses). Fails on a network/parse
/// error; the CLI's per-question loop treats that as a miss for this one question, not a fatal
/// error for the whole `--judge` run.
pub async fn judge_answer(
    judge_llm: &dyn Generator,
    question: &str,
    sources: &[SourceCitation],
    answer: &str,
    hint: Option<&str>,
) -> Result<JudgeVerdict> {
    let prompt = build_judge_prompt(question, sources, answer, hint);
    let raw = judge_llm
        .generate(&prompt)
        .await
        .context("judge LLM call failed")?;
    parse_judge_response(&raw).with_context(|| format!("unparseable judge response: {raw:?}"))
}

/// True if stored path `p` satisfies expected path `e`.
///
/// An **absolute** expect (`/…`) must match exactly — this preserves the original
/// semantics, keeps absolute golden files deterministic, and is what the existing
/// tilde-expanded `$HOME`-relative fixtures rely on. A **relative** expect (no
/// leading `/`) matches as a path-boundary suffix of `p`, so a committed fixture
/// can name `crates/query/src/eval.rs` and match wherever the repo is checked out
/// (CI's `/home/runner/work/...`, any developer's clone) without hardcoding an
/// absolute prefix. The boundary check (`/` before the suffix) stops `auth.rs`
/// from matching `oauth.rs`. POSIX-separator oriented — the relative form is for
/// portable POSIX fixtures, not Windows paths.
fn path_matches(p: &str, e: &str) -> bool {
    if e.starts_with('/') {
        return e == p;
    }
    p == e || (p.len() > e.len() && p.ends_with(e) && p.as_bytes()[p.len() - e.len() - 1] == b'/')
}

/// Pure scoring of a ranked path list against the expected set — split from
/// retrieval so the math is testable without a store. See [`path_matches`] for the
/// exact-vs-suffix matching rule (absolute = exact, relative = boundary suffix).
pub fn score_ranking(
    question: &str,
    k: usize,
    ranked_paths: &[&str],
    expect_paths: &[String],
) -> QuestionMetrics {
    let is_expected = |p: &str| expect_paths.iter().any(|e| path_matches(p, e));
    let top = &ranked_paths[..ranked_paths.len().min(k)];
    let first_hit_rank = top.iter().position(|p| is_expected(p)).map(|i| i + 1);
    let matched = top.iter().filter(|p| is_expected(p)).count();

    // recall@k: how many of the DISTINCT expected paths got covered by some top-k hit.
    // Denominator is the expected set (the authored relevant items), so a 2-path question
    // with one path retrieved scores 0.5. (precision's denominator is the returned hits.)
    let recall = if expect_paths.is_empty() {
        0.0
    } else {
        let covered = expect_paths
            .iter()
            .filter(|e| top.iter().any(|p| path_matches(p, e)))
            .count();
        covered as f64 / expect_paths.len() as f64
    };

    // nDCG@k (binary relevance): DCG of the expected hits in the top-k, normalized by the
    // ideal where the same number of relevant hits sit at ranks 1..matched. 1.0 = expected
    // hits packed at the top; drops as a relevant hit sinks below irrelevant ones — the
    // ranking-quality signal hit@k is blind to. rank = i+1, so log2(rank+1) = log2(i+2).
    let dcg: f64 = top
        .iter()
        .enumerate()
        .filter(|(_, p)| is_expected(p))
        .map(|(i, _)| 1.0 / ((i as f64) + 2.0).log2())
        .sum();
    let idcg: f64 = (0..matched).map(|i| 1.0 / ((i as f64) + 2.0).log2()).sum();
    let ndcg = if idcg > 0.0 { dcg / idcg } else { 0.0 };

    QuestionMetrics {
        question: question.to_owned(),
        k,
        retrieved: top.len(),
        hit: first_hit_rank.is_some(),
        first_hit_rank,
        reciprocal_rank: first_hit_rank.map_or(0.0, |r| 1.0 / r as f64),
        precision: if top.is_empty() {
            0.0
        } else {
            matched as f64 / top.len() as f64
        },
        recall,
        ndcg,
        judge: None,
    }
}

/// Aggregate per-question metrics into the run summary (all 0.0 for an empty run;
/// the CLI rejects empty golden files before getting here).
///
/// `mean_judge_score`/`judged_questions` are derived purely from whichever `per_question`
/// entries happen to carry a `judge` verdict — the caller doesn't tell `aggregate` whether
/// `--judge` was requested, it just reads what's there. Both stay `None` when no entry has a
/// verdict (the default, non-`--judge` case, and a `--judge` run where every judge call failed).
pub fn aggregate(per_question: &[QuestionMetrics]) -> EvalSummary {
    let n = per_question.len();
    let judge_scores: Vec<f64> = per_question
        .iter()
        .filter_map(|m| m.judge.as_ref().map(|j| f64::from(j.score)))
        .collect();
    let (mean_judge_score, judged_questions) = if judge_scores.is_empty() {
        (None, None)
    } else {
        let jn = judge_scores.len();
        (Some(judge_scores.iter().sum::<f64>() / jn as f64), Some(jn))
    };
    if n == 0 {
        return EvalSummary {
            questions: 0,
            hit_rate: 0.0,
            mrr: 0.0,
            mean_precision: 0.0,
            mean_recall: 0.0,
            mean_ndcg: 0.0,
            mean_judge_score,
            judged_questions,
        };
    }
    let nf = n as f64;
    EvalSummary {
        questions: n,
        hit_rate: per_question.iter().filter(|m| m.hit).count() as f64 / nf,
        mrr: per_question.iter().map(|m| m.reciprocal_rank).sum::<f64>() / nf,
        mean_precision: per_question.iter().map(|m| m.precision).sum::<f64>() / nf,
        mean_recall: per_question.iter().map(|m| m.recall).sum::<f64>() / nf,
        mean_ndcg: per_question.iter().map(|m| m.ndcg).sum::<f64>() / nf,
        mean_judge_score,
        judged_questions,
    }
}

/// One aggregate metric compared against a baseline run.
#[derive(Debug, Clone, Serialize)]
pub struct MetricDelta {
    pub name: &'static str,
    pub current: f64,
    pub baseline: f64,
    /// `current - baseline` (positive = improved).
    pub delta: f64,
    /// True when the drop exceeds the allowed tolerance (`delta < -max_regression`).
    pub regressed: bool,
}

/// Float-comparison guard for the regression gate: a drop smaller than this is treated as
/// noise (the baseline's f64 round-trips through JSON; summation order), never a regression.
/// `eval` is deterministic run-to-run, and a real regression moves a metric by ≫ this (a single
/// rank change shifts nDCG by ~1e-2), so this only absorbs sub-ULP serialize/parse jitter —
/// without it an identical baseline spuriously "regresses" by ~1e-16.
pub const REGRESSION_EPSILON: f64 = 1e-9;

/// Compare a run's aggregates against a baseline run, one [`MetricDelta`] per metric.
/// A metric `regressed` when it dropped by more than `max_regression` (so `0.0` = no drop
/// allowed, modulo [`REGRESSION_EPSILON`]). Pure + order-stable so the CLI can both print the
/// deltas and gate on them.
pub fn compare_to_baseline(
    current: &EvalSummary,
    baseline: &EvalSummary,
    max_regression: f64,
) -> Vec<MetricDelta> {
    [
        ("hit_rate", current.hit_rate, baseline.hit_rate),
        ("MRR", current.mrr, baseline.mrr),
        ("recall", current.mean_recall, baseline.mean_recall),
        ("nDCG", current.mean_ndcg, baseline.mean_ndcg),
        ("precision", current.mean_precision, baseline.mean_precision),
    ]
    .into_iter()
    .map(|(name, cur, base)| {
        let delta = cur - base;
        MetricDelta {
            name,
            current: cur,
            baseline: base,
            delta,
            regressed: delta < -(max_regression + REGRESSION_EPSILON),
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::config::HybridMode;
    use indexa_core::store::ChunkRecord;

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn score_ranking_hit_at_rank_one() {
        let m = score_ranking("q", 10, &["/a.md", "/b.md"], &owned(&["/a.md"]));
        assert!(m.hit);
        assert_eq!(m.first_hit_rank, Some(1));
        assert_eq!(m.reciprocal_rank, 1.0);
        assert_eq!(m.precision, 0.5);
        assert_eq!(m.retrieved, 2);
    }

    #[test]
    fn score_ranking_reciprocal_rank_of_later_hit() {
        let m = score_ranking("q", 10, &["/x.md", "/a.md"], &owned(&["/a.md"]));
        assert_eq!(m.first_hit_rank, Some(2));
        assert_eq!(m.reciprocal_rank, 0.5);
        assert_eq!(m.precision, 0.5);
    }

    #[test]
    fn score_ranking_k_truncates_before_scoring() {
        // The expected path is at rank 3 but k=2 cuts it off → a miss.
        let m = score_ranking("q", 2, &["/x.md", "/y.md", "/a.md"], &owned(&["/a.md"]));
        assert!(!m.hit);
        assert_eq!(m.first_hit_rank, None);
        assert_eq!(m.reciprocal_rank, 0.0);
        assert_eq!(m.precision, 0.0);
        assert_eq!(m.retrieved, 2);
    }

    #[test]
    fn score_ranking_precision_is_over_returned_hits() {
        // Both returned chunks are expected → precision 1.0 even though k is 10.
        let m = score_ranking("q", 10, &["/a.md", "/b.md"], &owned(&["/a.md", "/b.md"]));
        assert_eq!(m.precision, 1.0);
    }

    #[test]
    fn score_ranking_empty_results_score_zero() {
        let m = score_ranking("q", 10, &[], &owned(&["/a.md"]));
        assert!(!m.hit);
        assert_eq!(m.reciprocal_rank, 0.0);
        assert_eq!(m.precision, 0.0);
        assert_eq!(m.retrieved, 0);
    }

    #[test]
    fn path_matches_absolute_is_exact_only() {
        // Absolute expects keep the original exact-equality semantics.
        assert!(path_matches("/repo/src/auth.rs", "/repo/src/auth.rs"));
        assert!(!path_matches(
            "/home/x/repo/src/auth.rs",
            "/repo/src/auth.rs"
        ));
        assert!(!path_matches("/repo/src/oauth.rs", "/repo/src/auth.rs"));
    }

    #[test]
    fn path_matches_relative_is_boundary_suffix() {
        // A relative expect matches any checkout location at a `/` boundary.
        assert!(path_matches(
            "/home/runner/work/indexa/indexa/crates/query/src/eval.rs",
            "crates/query/src/eval.rs"
        ));
        assert!(path_matches(
            "/Users/dev/indexa/crates/query/src/eval.rs",
            "crates/query/src/eval.rs"
        ));
        // Equal-string relative match also holds.
        assert!(path_matches("eval.rs", "eval.rs"));
    }

    #[test]
    fn path_matches_relative_respects_path_boundary() {
        // Must not match mid-segment: `auth.rs` is not a suffix of `oauth.rs`.
        assert!(!path_matches("/repo/src/oauth.rs", "auth.rs"));
        assert!(path_matches("/repo/src/auth.rs", "auth.rs"));
        // A longer relative suffix still needs the `/` boundary.
        assert!(!path_matches("/repo/notsrc/eval.rs", "src/eval.rs"));
        assert!(path_matches("/repo/query/src/eval.rs", "src/eval.rs"));
    }

    #[test]
    fn score_ranking_relative_expect_matches_absolute_hit() {
        // End-to-end: a relative golden path scores a hit against an absolute
        // stored path — the property the portable self-golden fixture relies on.
        let m = score_ranking(
            "q",
            10,
            &["/home/runner/work/indexa/indexa/crates/query/src/eval.rs"],
            &owned(&["crates/query/src/eval.rs"]),
        );
        assert!(m.hit);
        assert_eq!(m.first_hit_rank, Some(1));
        assert_eq!(m.precision, 1.0);
    }

    #[test]
    fn score_ranking_recall_counts_distinct_expected() {
        // 2 expected, 1 in top-k → recall 0.5 (hit@k still true; recall is the graded view).
        let m = score_ranking("q", 10, &["/a.md", "/x.md"], &owned(&["/a.md", "/b.md"]));
        assert!(m.hit);
        assert!((m.recall - 0.5).abs() < 1e-9);
        // both expected retrieved → 1.0
        let m = score_ranking("q", 10, &["/a.md", "/b.md"], &owned(&["/a.md", "/b.md"]));
        assert!((m.recall - 1.0).abs() < 1e-9);
        // none retrieved → 0.0
        let m = score_ranking("q", 10, &["/x.md"], &owned(&["/a.md"]));
        assert_eq!(m.recall, 0.0);
    }

    #[test]
    fn score_ranking_ndcg_rewards_top_rank() {
        // Expected at rank 1 → perfect nDCG.
        let m = score_ranking("q", 10, &["/a.md", "/x.md"], &owned(&["/a.md"]));
        assert!((m.ndcg - 1.0).abs() < 1e-9);
        // Same hit demoted to rank 3 → nDCG = (1/log2 4)/(1/log2 2) = 0.5, while hit@k is blind.
        let m = score_ranking("q", 10, &["/x.md", "/y.md", "/a.md"], &owned(&["/a.md"]));
        assert!(m.hit);
        assert!((m.ndcg - 0.5).abs() < 1e-9);
        // No hit → 0.0.
        let m = score_ranking("q", 10, &["/x.md"], &owned(&["/a.md"]));
        assert_eq!(m.ndcg, 0.0);
    }

    #[test]
    fn aggregate_includes_recall_and_ndcg() {
        let per = [
            score_ranking("q1", 10, &["/a.md"], &owned(&["/a.md"])), // recall 1, ndcg 1
            score_ranking("q2", 10, &["/x.md", "/y.md", "/b.md"], &owned(&["/b.md"])), // recall 1, ndcg 0.5
        ];
        let s = aggregate(&per);
        assert!((s.mean_recall - 1.0).abs() < 1e-9);
        assert!((s.mean_ndcg - 0.75).abs() < 1e-9);
    }

    #[test]
    fn aggregate_averages_across_questions() {
        let per = [
            score_ranking("q1", 10, &["/a.md"], &owned(&["/a.md"])),
            score_ranking("q2", 10, &["/x.md", "/b.md"], &owned(&["/b.md"])),
            score_ranking("q3", 10, &["/x.md"], &owned(&["/c.md"])),
        ];
        let s = aggregate(&per);
        assert_eq!(s.questions, 3);
        assert!((s.hit_rate - 2.0 / 3.0).abs() < 1e-9);
        assert!((s.mrr - (1.0 + 0.5 + 0.0) / 3.0).abs() < 1e-9);
        assert!((s.mean_precision - (1.0 + 0.5 + 0.0) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn compare_to_baseline_flags_only_real_regressions() {
        let base = EvalSummary {
            questions: 10,
            hit_rate: 0.90,
            mrr: 0.80,
            mean_precision: 0.50,
            mean_recall: 0.70,
            mean_ndcg: 0.85,
            mean_judge_score: None,
            judged_questions: None,
        };
        // hit_rate drops 0.10, MRR improves, the rest unchanged.
        let cur = EvalSummary {
            hit_rate: 0.80,
            mrr: 0.85,
            ..base.clone()
        };
        // Zero tolerance: the 0.10 hit_rate drop regresses; the MRR improvement does not.
        let deltas = compare_to_baseline(&cur, &base, 0.0);
        let hit = deltas.iter().find(|d| d.name == "hit_rate").unwrap();
        assert!(hit.regressed);
        assert!((hit.delta + 0.10).abs() < 1e-9);
        assert!(!deltas.iter().find(|d| d.name == "MRR").unwrap().regressed);
        // A 0.10 tolerance absorbs the drop exactly at the boundary → nothing flagged.
        let deltas = compare_to_baseline(&cur, &base, 0.10);
        assert!(!deltas.iter().any(|d| d.regressed));
    }

    #[test]
    fn compare_to_baseline_ignores_float_roundtrip_noise() {
        // A sub-ULP drop (what an identical run shows after the baseline round-trips through
        // JSON) must NOT be flagged at zero tolerance — only real regressions are.
        let base = EvalSummary {
            questions: 18,
            hit_rate: 1.0,
            mrr: 1.0,
            mean_precision: 0.4,
            mean_recall: 0.97,
            mean_ndcg: 0.9736251154055859,
            mean_judge_score: None,
            judged_questions: None,
        };
        let cur = EvalSummary {
            mean_ndcg: base.mean_ndcg - 1e-15,
            ..base.clone()
        };
        let deltas = compare_to_baseline(&cur, &base, 0.0);
        assert!(
            !deltas.iter().any(|d| d.regressed),
            "sub-epsilon jitter must not count as a regression"
        );
    }

    #[test]
    fn aggregate_empty_run_is_all_zero() {
        let s = aggregate(&[]);
        assert_eq!(s.questions, 0);
        assert_eq!(s.hit_rate, 0.0);
    }

    // ── End-to-end against a real temp store (sparse / FTS, hermetic) ─────────

    fn temp_index(chunks: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut store = Store::open(&path).unwrap();
        let records: Vec<ChunkRecord> = chunks
            .iter()
            .map(|(p, text)| ChunkRecord {
                entry_path: (*p).to_owned(),
                seq: 0,
                heading: String::new(),
                text: (*text).to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
                content_hash: None,
            })
            .collect();
        store.upsert_chunks(&records).unwrap();
        (dir, path)
    }

    fn sparse_cfg() -> QaConfig {
        QaConfig {
            mode: HybridMode::Sparse,
            ..QaConfig::default()
        }
    }

    #[test]
    fn evaluate_question_scores_sparse_retrieval() {
        let (_dir, path) = temp_index(&[
            ("/code/auth.rs", "authentication session token login flow"),
            ("/code/db.rs", "database connection pooling sqlite"),
            ("/docs/auth.md", "authentication guide and setup"),
        ]);
        let store = Store::open(&path).unwrap();
        let cfg = sparse_cfg();

        // Distinct term → exactly one chunk matches, at rank 1.
        let q = EvalQuestion {
            question: "sqlite".to_owned(),
            expect_paths: owned(&["/code/db.rs"]),
            k: None,
            expect_answer_hint: None,
        };
        let m = evaluate_question(&store, &q, &cfg, None).unwrap();
        assert!(m.hit);
        assert_eq!(m.first_hit_rank, Some(1));
        assert_eq!(m.reciprocal_rank, 1.0);
        assert_eq!(m.precision, 1.0);

        // Both authentication chunks expected → full marks regardless of their
        // relative BM25 order.
        let q = EvalQuestion {
            question: "authentication".to_owned(),
            expect_paths: owned(&["/code/auth.rs", "/docs/auth.md"]),
            k: None,
            expect_answer_hint: None,
        };
        let m = evaluate_question(&store, &q, &cfg, None).unwrap();
        assert!(m.hit);
        assert_eq!(m.first_hit_rank, Some(1));
        assert_eq!(m.retrieved, 2);
        assert_eq!(m.precision, 1.0);

        // Matching content from the wrong file → retrieved but a miss.
        let q = EvalQuestion {
            question: "sqlite".to_owned(),
            expect_paths: owned(&["/docs/auth.md"]),
            k: None,
            expect_answer_hint: None,
        };
        let m = evaluate_question(&store, &q, &cfg, None).unwrap();
        assert!(!m.hit);
        assert_eq!(m.retrieved, 1);
        assert_eq!(m.precision, 0.0);

        // No FTS match at all → zero across the board.
        let q = EvalQuestion {
            question: "zebra".to_owned(),
            expect_paths: owned(&["/code/db.rs"]),
            k: None,
            expect_answer_hint: None,
        };
        let m = evaluate_question(&store, &q, &cfg, None).unwrap();
        assert!(!m.hit);
        assert_eq!(m.retrieved, 0);
    }

    #[test]
    fn evaluate_question_per_question_k_overrides_run_top_k() {
        let (_dir, path) = temp_index(&[
            ("/a.md", "kumquat orchard notes"),
            ("/b.md", "kumquat harvest schedule"),
        ]);
        let store = Store::open(&path).unwrap();
        let cfg = sparse_cfg(); // top_k 8

        let q = EvalQuestion {
            question: "kumquat".to_owned(),
            expect_paths: owned(&["/a.md", "/b.md"]),
            k: Some(1),
            expect_answer_hint: None,
        };
        let m = evaluate_question(&store, &q, &cfg, None).unwrap();
        assert_eq!(m.k, 1);
        assert_eq!(m.retrieved, 1, "k=1 must cap retrieval, not just scoring");
        assert!(m.hit);
    }

    // ── `evaluate_question_reranked` (`indexa eval --rerank`) ──────────────────

    /// A fake `Generator` that ignores the prompt entirely and always returns a fixed
    /// "reverse the two candidates" ranking — deterministic and prompt-format-independent
    /// (unlike inferring candidate count from the prompt text), so it can't be broken by an
    /// unrelated edit to `LlmReranker`'s prompt template.
    struct ReverseTwoRerankLlm;
    #[async_trait::async_trait]
    impl indexa_llm::Generator for ReverseTwoRerankLlm {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            Ok("2,1".to_owned())
        }
    }

    #[tokio::test]
    async fn evaluate_question_reranked_changes_ranking_when_rerank_is_set() {
        // Two chunks matching "widget" with very different term frequency, so plain BM25
        // retrieval has a clear, deterministic winner at rank 1 — whichever hit that is,
        // the fake reranker's fixed "2,1" reversal must flip it to rank 2 (and vice versa).
        let (_dir, path) = temp_index(&[
            ("/high.md", "widget widget widget widget widget"),
            ("/low.md", "widget appears exactly once here"),
        ]);
        let store = Store::open(&path).unwrap();
        let q = EvalQuestion {
            question: "widget".to_owned(),
            expect_paths: owned(&["/low.md"]),
            k: None,
            expect_answer_hint: None,
        };

        let baseline = evaluate_question(&store, &q, &sparse_cfg(), None).unwrap();
        assert_eq!(baseline.retrieved, 2, "fixture must retrieve both chunks");
        assert!(
            baseline.hit,
            "expected path must be retrieved to prove rerank moved its rank"
        );

        let mut rerank_cfg = sparse_cfg();
        rerank_cfg.rerank = true;
        rerank_cfg.rerank_backend = "llm".to_owned();
        let reranked =
            evaluate_question_reranked(&store, &q, &rerank_cfg, None, &ReverseTwoRerankLlm)
                .await
                .unwrap();
        assert_eq!(reranked.retrieved, 2);
        assert!(reranked.hit);
        assert_ne!(
            baseline.first_hit_rank, reranked.first_hit_rank,
            "the rerank pass must actually reorder the hits, not just thread through inert"
        );
    }

    #[tokio::test]
    async fn evaluate_question_reranked_is_a_noop_when_rerank_is_off() {
        // `cfg.rerank == false` — the CLI's un-flagged default (`sparse_cfg()`/`QaConfig::default()`
        // has `rerank: true`, matching production `ask`; the CLI explicitly overrides it to `false`
        // unless `--rerank` is passed) — must score byte-identically to `evaluate_question`, proving
        // `evaluate_question_reranked` is additive: passing the same fake reranker has zero effect
        // when the flag it's gated on is off.
        let (_dir, path) = temp_index(&[
            ("/high.md", "widget widget widget widget widget"),
            ("/low.md", "widget appears exactly once here"),
        ]);
        let store = Store::open(&path).unwrap();
        let q = EvalQuestion {
            question: "widget".to_owned(),
            expect_paths: owned(&["/low.md"]),
            k: None,
            expect_answer_hint: None,
        };

        let mut no_rerank_cfg = sparse_cfg();
        no_rerank_cfg.rerank = false;

        let baseline = evaluate_question(&store, &q, &no_rerank_cfg, None).unwrap();
        let via_reranked_fn =
            evaluate_question_reranked(&store, &q, &no_rerank_cfg, None, &ReverseTwoRerankLlm)
                .await
                .unwrap();
        assert_eq!(baseline.first_hit_rank, via_reranked_fn.first_hit_rank);
        assert_eq!(baseline.precision, via_reranked_fn.precision);
    }

    /// Live dense/RRF A/B eval over the committed golden set against a populated index. The CI gate
    /// scores sparse-only (hermetic, no Ollama), so it can't see an embedding change; this is the
    /// opt-in counterpart that *can* — run it on `main` and on a branch to prove a contextual-prefix
    /// embedding change doesn't regress recall/nDCG before promoting it to default. Deliberately
    /// calls bare `evaluate_question` with `rerank: false` (not [`evaluate_question_reranked`]) to
    /// isolate the embedding/retrieval measurement from an extra LLM call; validating a reranker
    /// swap is `evaluate_question_reranked` / `indexa eval --rerank`'s job, covered by the
    /// `evaluate_question_reranked_*` unit tests below (fake in-process `Generator`, no network).
    /// `#[ignore]`d: needs a real Ollama (`nomic-embed-text`) + a populated index.
    ///
    /// ```bash
    /// # uses the macOS default index unless INDEXA_TEST_INDEX_DB is set
    /// cargo test -p indexa-query dense_rrf_eval_over_golden -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "dense A/B; needs Ollama (nomic-embed-text) + a populated index; run with --ignored --nocapture"]
    async fn dense_rrf_eval_over_golden() {
        use indexa_embed::Embedder;

        let db = std::env::var("INDEXA_TEST_INDEX_DB")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join("Library/Application Support/dev.indexa.Indexa/index.db")
            });
        if !db.exists() {
            eprintln!("SKIP: no index at {db:?} (set INDEXA_TEST_INDEX_DB)");
            return;
        }

        // The committed golden set, located relative to this crate so it works from any checkout.
        let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/self-golden.json");
        let raw = std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|e| panic!("read golden {golden_path:?}: {e}"));
        let golden: GoldenSet = serde_json::from_str(&raw).expect("parse golden json");

        let store = Store::open(&db).unwrap();
        let embedder =
            indexa_embed::OllamaEmbedder::new("http://localhost:11434", "nomic-embed-text", 768);
        // RRF fuses dense + sparse; rerank off keeps this an embedding/retrieval measurement (no LLM).
        let cfg = QaConfig {
            mode: HybridMode::Rrf,
            top_k: 10,
            rerank: false,
            ..QaConfig::default()
        };

        let mut per_question = Vec::with_capacity(golden.questions.len());
        for q in &golden.questions {
            let vec = embedder.embed(&q.question).await.expect("embed question");
            per_question.push(evaluate_question(&store, q, &cfg, Some(&vec)).unwrap());
        }
        let summary = aggregate(&per_question);
        eprintln!(
            "dense/RRF over {} golden Qs: hit_rate={:.3} mrr={:.3} recall={:.3} ndcg={:.3}",
            summary.questions,
            summary.hit_rate,
            summary.mrr,
            summary.mean_recall,
            summary.mean_ndcg
        );
        // Loose floor: dense hybrid over the self-index should comfortably clear 0.5.
        assert!(
            summary.hit_rate >= 0.5,
            "dense hit_rate {:.3} below floor 0.5 — retrieval regression?",
            summary.hit_rate
        );
    }

    // ── `--judge` mode: prompt construction, response parsing, aggregation ─────

    fn src(path: &str, heading: &str, snippet: &str) -> SourceCitation {
        SourceCitation {
            path: path.to_owned(),
            heading: heading.to_owned(),
            snippet: snippet.to_owned(),
        }
    }

    #[test]
    fn build_judge_prompt_includes_hint_only_when_present() {
        let sources = [src("/a.rs", "", "some snippet")];
        let without = build_judge_prompt("q?", &sources, "answer text", None);
        assert!(!without.contains("should mention"));

        let with = build_judge_prompt("q?", &sources, "answer text", Some("mention RRF"));
        assert!(with.contains("should mention: mention RRF"));
    }

    #[test]
    fn build_judge_prompt_includes_question_sources_and_answer() {
        let sources = [src("/code/auth.rs", "Login", "auth flow details")];
        let prompt = build_judge_prompt("how does login work?", &sources, "It uses tokens.", None);
        assert!(prompt.contains("how does login work?"));
        assert!(prompt.contains("/code/auth.rs — Login"));
        assert!(prompt.contains("auth flow details"));
        assert!(prompt.contains("It uses tokens."));
    }

    #[test]
    fn build_judge_prompt_marks_empty_sources() {
        let prompt = build_judge_prompt("q?", &[], "answer", None);
        assert!(prompt.contains("(none retrieved)"));
    }

    #[test]
    fn parse_judge_response_reads_score_and_reason() {
        let v = parse_judge_response("SCORE: 4\nREASON: Mostly supported, one minor gap.").unwrap();
        assert_eq!(v.score, 4);
        assert_eq!(v.reason, "Mostly supported, one minor gap.");
    }

    #[test]
    fn parse_judge_response_tolerates_lowercase_and_preamble() {
        // Real models don't always obey "nothing else" — a stray preamble line must not break
        // parsing, and lowercase `score:`/`reason:` must still be recognized.
        let v = parse_judge_response(
            "Sure, here is my grading:\nscore: 3\nreason: partially addresses the question.",
        )
        .unwrap();
        assert_eq!(v.score, 3);
        assert_eq!(v.reason, "partially addresses the question.");
    }

    #[test]
    fn parse_judge_response_tolerates_a_same_line_preamble_before_score() {
        // Regression: REASON: was already tolerant of a same-line preamble (`.find`), but
        // SCORE: required `strip_prefix` at the start of the line — inconsistent, and a model
        // that writes "Final score: 4" instead of a bare "SCORE: 4" line used to fail to parse
        // at all (no score found).
        let v = parse_judge_response("Final score: 4\nREASON: solid citation coverage.").unwrap();
        assert_eq!(v.score, 4);
        assert_eq!(v.reason, "solid citation coverage.");
    }

    #[test]
    fn parse_judge_response_clamps_out_of_range_score() {
        assert_eq!(
            parse_judge_response("SCORE: 9\nREASON: overclaimed.")
                .unwrap()
                .score,
            5
        );
        assert_eq!(
            parse_judge_response("SCORE: -3\nREASON: way off.")
                .unwrap()
                .score,
            0
        );
    }

    #[test]
    fn parse_judge_response_missing_score_is_an_error() {
        assert!(parse_judge_response("REASON: no score given here.").is_err());
    }

    #[test]
    fn parse_judge_response_missing_reason_falls_back() {
        let v = parse_judge_response("SCORE: 2").unwrap();
        assert_eq!(v.score, 2);
        assert_eq!(v.reason, "(no reason given)");
    }

    /// A fake `Generator` that always returns a fixed, well-formed judge verdict — proves
    /// `judge_answer` wires the prompt → LLM call → parse pipeline end to end.
    struct FixedVerdictLlm(&'static str);
    #[async_trait::async_trait]
    impl indexa_llm::Generator for FixedVerdictLlm {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    #[tokio::test]
    async fn judge_answer_end_to_end_with_fake_llm() {
        let llm = FixedVerdictLlm("SCORE: 5\nREASON: Fully supported by the cited source.");
        let sources = [src("/docs/api.md", "", "the API returns JSON")];
        let v = judge_answer(
            &llm,
            "what format does the API return?",
            &sources,
            "JSON.",
            None,
        )
        .await
        .unwrap();
        assert_eq!(v.score, 5);
        assert_eq!(v.reason, "Fully supported by the cited source.");
    }

    /// A fake `Generator` that returns garbage — proves a judge parse failure surfaces as an
    /// `Err` (the CLI's per-question loop turns that into a skipped verdict, not a fatal error).
    struct GarbageLlm;
    #[async_trait::async_trait]
    impl indexa_llm::Generator for GarbageLlm {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            Ok("I refuse to grade this.".to_owned())
        }
    }

    #[tokio::test]
    async fn judge_answer_propagates_unparseable_response_as_error() {
        let err = judge_answer(&GarbageLlm, "q?", &[], "answer", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unparseable judge response"));
    }

    #[test]
    fn aggregate_computes_mean_judge_score_only_from_present_verdicts() {
        let mut per = [
            score_ranking("q1", 10, &["/a.md"], &owned(&["/a.md"])),
            score_ranking("q2", 10, &["/b.md"], &owned(&["/b.md"])),
            score_ranking("q3", 10, &["/c.md"], &owned(&["/c.md"])),
        ];
        // q1 and q3 got graded (4, 2); q2's synthesis/judge call failed and stayed `None`.
        per[0].judge = Some(JudgeVerdict {
            score: 4,
            reason: "ok".to_owned(),
        });
        per[2].judge = Some(JudgeVerdict {
            score: 2,
            reason: "weak".to_owned(),
        });
        let s = aggregate(&per);
        assert_eq!(s.judged_questions, Some(2));
        assert!((s.mean_judge_score.unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_leaves_judge_fields_none_without_judge_mode() {
        // The default (non-`--judge`) path: no entry carries a verdict, so both fields stay
        // `None` — this is what keeps a plain run's `--json` output unchanged.
        let per = [score_ranking("q1", 10, &["/a.md"], &owned(&["/a.md"]))];
        let s = aggregate(&per);
        assert!(s.mean_judge_score.is_none());
        assert!(s.judged_questions.is_none());
    }

    #[test]
    fn eval_summary_without_judge_fields_deserializes_with_none() {
        // An OLD saved baseline (written before this field existed) must still deserialize into
        // the CURRENT EvalSummary — the whole point of `#[serde(default)]` on the new fields.
        let old_json = r#"{
            "questions": 5,
            "hit_rate": 0.8,
            "mrr": 0.7,
            "mean_precision": 0.5,
            "mean_recall": 0.6,
            "mean_ndcg": 0.75
        }"#;
        let s: EvalSummary = serde_json::from_str(old_json).unwrap();
        assert_eq!(s.questions, 5);
        assert!(s.mean_judge_score.is_none());
        assert!(s.judged_questions.is_none());

        // And it still compares against a current summary fine.
        let current = aggregate(&[score_ranking("q1", 10, &["/a.md"], &owned(&["/a.md"]))]);
        let deltas = compare_to_baseline(&current, &s, 1.0);
        assert_eq!(
            deltas.len(),
            5,
            "judge score is not part of the baseline delta set"
        );
    }

    #[test]
    fn eval_question_without_hint_field_still_parses() {
        // Backward compatibility for the golden schema: a question with no `expect_answer_hint`
        // (every existing golden file, including fixtures/self-golden.json) still parses, with
        // the field defaulting to `None`.
        let set: GoldenSet = serde_json::from_str(
            r#"{"questions": [{"question": "q", "expect_paths": ["/a.rs"]}]}"#,
        )
        .unwrap();
        assert!(set.questions[0].expect_answer_hint.is_none());
    }

    #[test]
    fn eval_question_with_hint_field_parses() {
        let set: GoldenSet = serde_json::from_str(
            r#"{"questions": [{"question": "q", "expect_paths": ["/a.rs"], "expect_answer_hint": "must mention foo"}]}"#,
        )
        .unwrap();
        assert_eq!(
            set.questions[0].expect_answer_hint.as_deref(),
            Some("must mention foo")
        );
    }
}
