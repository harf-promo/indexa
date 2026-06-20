//! Retrieval-quality evaluation backing `indexa eval`.
//!
//! Scores the exact `retrieve()` ranking the ask pipeline uses (hybrid search +
//! summary/importance boosts) against a golden-questions file — retrieval only,
//! no LLM synthesis, so a sparse-mode run is deterministic and needs no Ollama.
//! This is the regression gate for retrieval-affecting changes (chunking,
//! parsing, ranking).

use anyhow::Result;
use indexa_core::store::Store;
use serde::{Deserialize, Serialize};

use crate::qa::{retrieve, QaConfig};

/// One golden question: a query plus the file paths a correct retrieval must surface.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalQuestion {
    pub question: String,
    /// Paths exactly as stored in the index (absolute; the CLI tilde-expands them).
    pub expect_paths: Vec<String>,
    /// Per-question cutoff; falls back to the run-level top-k when unset.
    #[serde(default)]
    pub k: Option<usize>,
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
    }
}

/// Aggregate per-question metrics into the run summary (all 0.0 for an empty run;
/// the CLI rejects empty golden files before getting here).
pub fn aggregate(per_question: &[QuestionMetrics]) -> EvalSummary {
    let n = per_question.len();
    if n == 0 {
        return EvalSummary {
            questions: 0,
            hit_rate: 0.0,
            mrr: 0.0,
            mean_precision: 0.0,
            mean_recall: 0.0,
            mean_ndcg: 0.0,
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
        };
        let m = evaluate_question(&store, &q, &cfg, None).unwrap();
        assert_eq!(m.k, 1);
        assert_eq!(m.retrieved, 1, "k=1 must cap retrieval, not just scoring");
        assert!(m.hit);
    }
}
