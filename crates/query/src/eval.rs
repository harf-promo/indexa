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
}

/// Aggregate over all questions in a run.
#[derive(Debug, Clone, Serialize)]
pub struct EvalSummary {
    pub questions: usize,
    /// Fraction of questions with at least one expected path in their top k (hit@k).
    pub hit_rate: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
    pub mean_precision: f64,
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

/// Pure scoring of a ranked path list against the expected set — split from
/// retrieval so the math is testable without a store. Paths match by exact
/// string equality (the CLI normalizes tildes before calling).
pub fn score_ranking(
    question: &str,
    k: usize,
    ranked_paths: &[&str],
    expect_paths: &[String],
) -> QuestionMetrics {
    let is_expected = |p: &str| expect_paths.iter().any(|e| e == p);
    let top = &ranked_paths[..ranked_paths.len().min(k)];
    let first_hit_rank = top.iter().position(|p| is_expected(p)).map(|i| i + 1);
    let matched = top.iter().filter(|p| is_expected(p)).count();
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
        };
    }
    let nf = n as f64;
    EvalSummary {
        questions: n,
        hit_rate: per_question.iter().filter(|m| m.hit).count() as f64 / nf,
        mrr: per_question.iter().map(|m| m.reciprocal_rank).sum::<f64>() / nf,
        mean_precision: per_question.iter().map(|m| m.precision).sum::<f64>() / nf,
    }
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
