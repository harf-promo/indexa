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
    /// Salient question terms that appear in NONE of the retrieved sources — a heuristic
    /// "the index may not cover these aspects" hint (see [`compute_uncovered`]). `None`
    /// when every salient term was covered (or the question had none). Populated by
    /// [`confidence_for`]; [`assess_confidence`] alone leaves it `None` (it has no question).
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
/// when the mode embedded the query (everything but sparse-only). Also fills in
/// `uncovered` — salient question terms absent from every retrieved source.
pub(crate) fn confidence_for(
    hits: &[SearchHit],
    cfg: &QaConfig,
    question: &str,
) -> Option<ConfidenceReport> {
    let mut report = assess_confidence(
        hits,
        cfg.top_k,
        cfg.rrf_k,
        !matches!(cfg.mode, HybridMode::Sparse),
    )?;
    report.uncovered = compute_uncovered(question, hits);
    Some(report)
}

/// Salient (content) terms of a question: lowercased alphanumeric tokens ≥ 4 chars,
/// minus a small stop-list of question/instruction words. Deduped, order-preserving.
fn salient_terms(question: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "what", "where", "when", "which", "whom", "whose", "does", "did", "the", "this", "that",
        "these", "those", "with", "from", "into", "about", "there", "here", "have", "has", "had",
        "your", "you", "our", "and", "but", "for", "not", "can", "could", "would", "should",
        "will", "shall", "explain", "tell", "show", "find", "list", "give", "describe", "please",
        "file", "files", "code", "using", "used", "their", "they", "them", "work", "works",
        "working", "make", "makes", "mean", "means", "happen", "happens",
    ];
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in question.split(|c: char| !c.is_alphanumeric()) {
        let t = raw.to_lowercase();
        if t.len() < 4 || STOP.contains(&t.as_str()) {
            continue;
        }
        if seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// Heuristic "uncovered aspects": salient question terms that appear in NONE of the
/// retrieved sources' path/heading/text. A cheap signal that the answer may be partial.
/// Returns `None` when nothing salient is missing (the common case). Capped at 5.
fn compute_uncovered(question: &str, hits: &[SearchHit]) -> Option<Vec<String>> {
    let terms = salient_terms(question);
    if terms.is_empty() {
        return None;
    }
    let mut hay = String::new();
    for h in hits {
        hay.push_str(&h.entry_path.to_lowercase());
        hay.push(' ');
        hay.push_str(&h.heading.to_lowercase());
        hay.push(' ');
        hay.push_str(&h.text.to_lowercase());
        hay.push(' ');
    }
    let missing: Vec<String> = terms
        .into_iter()
        .filter(|t| !hay.contains(t.as_str()))
        .take(5)
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

#[cfg(test)]
mod uncovered_tests {
    use super::*;

    fn hit_with(path: &str, text: &str) -> SearchHit {
        SearchHit {
            chunk_id: 1,
            entry_path: path.to_owned(),
            seq: 0,
            heading: String::new(),
            text: text.to_owned(),
            rrf_score: 0.1,
        }
    }

    #[test]
    fn flags_salient_terms_absent_from_all_sources() {
        let hits = vec![hit_with("/src/auth.rs", "login and session handling")];
        // "authentication" is covered (substring of nothing — actually not present), "billing"
        // is absent. "retrieval" word "payments" absent. Stop/short words ignored.
        let u = compute_uncovered("how does billing and session work?", &hits).unwrap();
        assert!(u.contains(&"billing".to_string()));
        assert!(
            !u.contains(&"session".to_string()),
            "covered term must not be flagged"
        );
    }

    #[test]
    fn none_when_everything_covered_or_no_salient_terms() {
        let hits = vec![hit_with("/src/auth.rs", "session login authentication")];
        assert!(compute_uncovered("how does session login work?", &hits).is_none());
        // No salient terms (all short/stop words) ⇒ None, never an empty list.
        assert!(compute_uncovered("what is it?", &hits).is_none());
    }
}
