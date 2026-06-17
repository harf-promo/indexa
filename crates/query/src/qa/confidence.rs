//! Retrieval-shape confidence classification.
//!
//! Pure, deterministic scoring of the *shape of the retrieval pool* — how well the
//! index covered a question — independent of the synthesized prose. Not calibrated;
//! the thresholds in [`assess_confidence`] are documented judgment calls. Part of the
//! [`qa`](super) pipeline (see [`crate::qa`] for the module map).

use indexa_core::config::HybridMode;
use indexa_core::store::SearchHit;

use super::QaConfig;

/// Heuristic answer-level confidence. Derived purely from the *shape of the
/// retrieval pool* before synthesis — it says how well the index covered the
/// question, not whether the model's prose is correct. NOT calibrated: the
/// thresholds in [`assess_confidence`] are documented judgment calls, not
/// probabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ConfidenceReport {
    pub level: Confidence,
    /// One-line human explanation, e.g. "9 strong matches".
    pub basis: String,
    /// The raw numbers the level was derived from (`indexa ask --explain` prints them).
    pub inputs: ConfidenceInputs,
    /// Phase-2 placeholder: question aspects retrieval likely did not cover.
    /// Always `None` today.
    pub uncovered: Option<Vec<String>>,
}

/// The retrieval-shape numbers behind a [`ConfidenceReport`], surfaced by
/// `indexa ask --explain` so a user can see why a level was chosen.
#[derive(Debug, Clone)]
pub struct ConfidenceInputs {
    pub hit_count: usize,
    pub top_k: usize,
    pub top_score: f64,
    pub median_score: f64,
    /// top/median score ratio (≥ 1.0): large ⇒ one dominant hit, ~1 ⇒ a flat pool.
    pub gap: f64,
    /// Hits at or above `strong_floor`.
    pub strong_hits: usize,
    /// Fused-mass floor for a "strong" hit: `1/(rrf_k+10)` ≈ top-10 in one retriever.
    pub strong_floor: f64,
    /// Whether dense retrieval ran (a query embedding existed), i.e. corroboration
    /// between keyword and semantic rankings was possible at all.
    pub embeddings: bool,
}

/// Classify retrieval-pool shape into a [`ConfidenceReport`]. Pure and deterministic;
/// `None` only for an empty pool (the no-match short-circuit speaks for itself).
///
/// Anchors derive from the RRF formula — a hit at rank `r` in one retriever
/// contributes `1/(rrf_k + r)` fused mass — so thresholds track `rrf_k` and the
/// *relative* structure of the pool rather than absolute magic numbers:
/// - `rank1` = `1/(rrf_k+1)`: a clean rank-1 in a single retriever.
/// - strong hit: ≥ `1/(rrf_k+10)` (≈ top-10 in one retriever, or equivalent fused mass).
/// - corroborated top (hybrid only): ≥ 1.5 × `rank1`, reachable only when keyword and
///   semantic retrieval both rank the same chunk near their tops (an importance weight
///   can also push a hit there — accepted, it encodes user judgment).
///
/// Levels (heuristic, NOT calibrated):
/// - High: corroborated top + ≥ 3 strong hits + pool at least half of `top_k` + no
///   single hit dominating a weak remainder (gap ≤ 3).
/// - Low: no strong hits at all, or a single weak hit.
/// - Medium: everything in between.
pub fn assess_confidence(
    hits: &[SearchHit],
    top_k: usize,
    rrf_k: f32,
    embeddings: bool,
) -> Option<ConfidenceReport> {
    if hits.is_empty() {
        return None;
    }
    // Sort scores locally: callers may hand us reranked (reordered) hits, and the
    // shape metrics must not depend on presentation order.
    let mut scores: Vec<f64> = hits.iter().map(|h| h.rrf_score).collect();
    scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let n = scores.len();
    let top = scores[0];
    let median = scores[n / 2].max(f64::EPSILON);
    let gap = top / median;

    let rank1 = 1.0 / (rrf_k as f64 + 1.0);
    let strong_floor = 1.0 / (rrf_k as f64 + 10.0);
    let strong = scores.iter().filter(|s| **s >= strong_floor).count();

    // Sparse-only can never corroborate, so its best possible evidence — a clean
    // keyword rank-1 — counts as a strong top there.
    let top_is_strong = if embeddings {
        top >= 1.5 * rank1
    } else {
        top >= rank1
    };

    let level = if strong == 0 {
        Confidence::Low
    } else if n == 1 {
        // A single chunk of evidence is never High, however well it scored.
        if top >= rank1 {
            Confidence::Medium
        } else {
            Confidence::Low
        }
    } else if top_is_strong && strong >= 3 && n * 2 >= top_k && gap <= 3.0 {
        Confidence::High
    } else {
        Confidence::Medium
    };

    let basis = match level {
        Confidence::High => format!("{strong} strong matches"),
        Confidence::Medium if n == 1 => "a single strong match — uncorroborated".to_owned(),
        Confidence::Medium if !top_is_strong => format!("{n} moderate matches"),
        Confidence::Medium if gap > 3.0 => "one dominant match, weak support".to_owned(),
        Confidence::Medium => format!(
            "only {strong} strong match{}",
            if strong == 1 { "" } else { "es" }
        ),
        Confidence::Low => {
            if n <= 2 {
                "few weak matches — the index may not cover this".to_owned()
            } else {
                "only weak matches — the index may not cover this".to_owned()
            }
        }
    };

    Some(ConfidenceReport {
        level,
        basis,
        inputs: ConfidenceInputs {
            hit_count: n,
            top_k,
            top_score: top,
            median_score: median,
            gap,
            strong_hits: strong,
            strong_floor,
            embeddings,
        },
        uncovered: None,
    })
}

/// [`assess_confidence`] wired to a [`QaConfig`]: embeddings were available exactly
/// when the mode embedded the query (everything but sparse-only).
pub(crate) fn confidence_for(hits: &[SearchHit], cfg: &QaConfig) -> Option<ConfidenceReport> {
    assess_confidence(
        hits,
        cfg.top_k,
        cfg.rrf_k,
        !matches!(cfg.mode, HybridMode::Sparse),
    )
}
