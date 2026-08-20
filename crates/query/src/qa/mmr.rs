//! MMR (Maximal Marginal Relevance) diversity re-ranking.
//!
//! Greedy selection balancing relevance against similarity to already-picked chunks,
//! applied by [`retrieve`](super::retrieve::retrieve) after all score boosts. Fails open.

use std::collections::HashMap;

use indexa_core::store::SearchHit;

/// Cosine similarity between two equal-length f32 vectors.
/// Returns 0.0 when either vector has zero norm (rather than NaN).
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Min-max normalize raw RRF scores to `[0, 1]` across one candidate pool.
///
/// `rrf_score` (`~1/61 .. ~0.05` at typical `rrf_k`) and cosine similarity (`[0, 1]`)
/// live on incompatible scales — combining them raw makes `lambda` mean nothing close
/// to its documented "0.5 = balanced" contract, since the relevance term is ~20-60x
/// smaller than the diversity term it's supposed to be balanced against. Normalizing
/// per-pool (not globally, since RRF scores aren't comparable across queries anyway)
/// fixes that without changing what "more relevant" means within this pool.
///
/// When every candidate has the same score (min == max — happens when the pool is a
/// single result, or came from one arm of the fusion), there's nothing to differentiate
/// on: mapping all to `1.0` keeps every candidate's relevance term equal and lets the
/// diversity term (which still varies per-candidate) drive the ranking, instead of
/// dividing by zero.
fn normalize_relevance(candidates: &[SearchHit]) -> HashMap<i64, f32> {
    let (min, max) = candidates
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), c| {
            let s = c.rrf_score as f32;
            (lo.min(s), hi.max(s))
        });
    let span = max - min;
    candidates
        .iter()
        .map(|c| {
            let norm = if span > f32::EPSILON {
                (c.rrf_score as f32 - min) / span
            } else {
                1.0
            };
            (c.chunk_id, norm)
        })
        .collect()
}

/// MMR score for one candidate chunk.
///
/// `mmr = λ * relevance - (1 - λ) * max_sim_to_selected`
///
/// `relevance` here is the pool-normalized RRF score from [`normalize_relevance`], not
/// the raw score — see that function's doc for why.
///
/// When `selected` is empty (no chunk chosen yet) the diversity penalty is zero,
/// so the first pick is always the highest-relevance chunk.
fn mmr_score(
    hit: &SearchHit,
    selected: &[&[f32]],
    lambda: f32,
    embeddings: &HashMap<i64, Vec<f32>>,
    norm_relevance: &HashMap<i64, f32>,
) -> f32 {
    let rel = norm_relevance.get(&hit.chunk_id).copied().unwrap_or(0.0);
    if selected.is_empty() {
        return rel;
    }
    let max_sim = match embeddings.get(&hit.chunk_id) {
        Some(v) => selected
            .iter()
            .map(|s| cosine(v, s))
            .fold(f32::NEG_INFINITY, f32::max),
        None => 0.0, // no embedding → no penalty (fail-open)
    };
    lambda * rel - (1.0 - lambda) * max_sim
}

/// Greedy MMR selection over `candidates`.
///
/// Each iteration picks the candidate with the highest MMR score (relevance
/// balanced against max similarity to already-selected items), adds it to the
/// result, and repeats until the candidate pool is exhausted.
///
/// **Early returns (no re-ordering):**
/// - `lambda >= 1.0` — pure relevance, MMR is a no-op.
/// - Fewer than 2 candidates — nothing to re-order.
/// - `embeddings` is empty — no vectors to compute similarity with.
pub(crate) fn apply_mmr(
    mut candidates: Vec<SearchHit>,
    embeddings: &HashMap<i64, Vec<f32>>,
    lambda: f32,
) -> Vec<SearchHit> {
    if lambda >= 1.0 || candidates.len() < 2 || embeddings.is_empty() {
        return candidates;
    }
    let norm_relevance = normalize_relevance(&candidates);
    let mut selected_vecs: Vec<&[f32]> = Vec::with_capacity(candidates.len());
    let mut result = Vec::with_capacity(candidates.len());

    // Greedy MMR selection — O(n²) in the number of candidates; at top_k=8..20
    // this is negligible.
    while !candidates.is_empty() {
        let best_idx = candidates
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let sa = mmr_score(a, &selected_vecs, lambda, embeddings, &norm_relevance);
                let sb = mmr_score(b, &selected_vecs, lambda, embeddings, &norm_relevance);
                sa.total_cmp(&sb)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);

        let hit = candidates.remove(best_idx);
        // Record the selected embedding so subsequent picks are penalised for
        // similarity to it. If no embedding exists for this chunk, push nothing —
        // future picks won't be penalised relative to it (safe fail-open).
        if let Some(v) = embeddings.get(&hit.chunk_id) {
            // SAFETY: `embeddings` is a `&HashMap` borrowed for the life of this
            // function, so the slice reference is valid for the whole loop.
            selected_vecs.push(v.as_slice());
        }
        result.push(hit);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(chunk_id: i64, rrf_score: f64) -> SearchHit {
        SearchHit {
            chunk_id,
            entry_path: format!("/f{chunk_id}.rs"),
            seq: 0,
            heading: String::new(),
            text: String::new(),
            rrf_score,
        }
    }

    #[test]
    fn normalize_relevance_maps_pool_to_zero_one() {
        let hits = vec![hit(1, 0.01), hit(2, 0.03), hit(3, 0.02)];
        let norm = normalize_relevance(&hits);
        assert_eq!(norm[&1], 0.0);
        assert_eq!(norm[&2], 1.0);
        assert!((norm[&3] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn normalize_relevance_all_equal_maps_to_one_not_nan() {
        // min == max would divide by zero under a naive (x-min)/(max-min); every
        // candidate must land on a defined constant instead.
        let hits = vec![hit(1, 0.02), hit(2, 0.02)];
        let norm = normalize_relevance(&hits);
        assert_eq!(norm[&1], 1.0);
        assert_eq!(norm[&2], 1.0);
    }

    #[test]
    fn apply_mmr_relevance_and_diversity_are_now_comparable_scales() {
        // Reproduces the realistic scale that motivated the fix: RRF scores in the
        // ~0.016-0.05 range (rrf_k=60, rank 0-2), embeddings with cosine similarity in
        // [0,1]. At the documented "balanced" lambda=0.5, a candidate that is far MORE
        // relevant (top RRF rank) than a near-duplicate of the first pick must still be
        // able to win over the near-duplicate — under the old raw-score mixing, the
        // diversity term (~0.3-1.0 scale) dwarfed the relevance term (~0.01-0.05 scale)
        // by 20-60x regardless of lambda, so this ordering could not be respected.
        let mut embeddings = HashMap::new();
        embeddings.insert(1i64, vec![1.0f32, 0.0]); // first pick
        embeddings.insert(2i64, vec![0.99f32, 0.14]); // near-duplicate of #1, low relevance
        embeddings.insert(3i64, vec![0.0f32, 1.0]); // orthogonal, but also low relevance

        let candidates = vec![
            hit(1, 1.0 / 61.0), // rank 0, most relevant — picked first regardless
            hit(2, 1.0 / 63.0), // rank 2, near-duplicate embedding of #1
            hit(3, 1.0 / 64.0), // rank 3, slightly less relevant than #2 but orthogonal
        ];

        let result = apply_mmr(candidates, &embeddings, 0.5);
        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0].chunk_id, 1,
            "highest relevance always picked first"
        );
        // With scores comparable, the diversity penalty on #2 (near-duplicate of #1)
        // must be enough to let the orthogonal-but-slightly-less-relevant #3 win second
        // pick — this is the property that was structurally impossible before the fix.
        assert_eq!(
            result[1].chunk_id, 3,
            "orthogonal candidate should out-rank a near-duplicate at a balanced lambda"
        );
    }

    #[test]
    fn apply_mmr_lambda_near_one_still_favors_relevance() {
        // lambda close to (but under) 1.0 should behave close to pure-relevance
        // ordering even with normalization in play.
        let mut embeddings = HashMap::new();
        embeddings.insert(1i64, vec![1.0f32, 0.0]);
        embeddings.insert(2i64, vec![1.0f32, 0.0]); // identical to #1
        let candidates = vec![hit(1, 0.05), hit(2, 0.01)];
        let result = apply_mmr(candidates, &embeddings, 0.99);
        assert_eq!(result[0].chunk_id, 1);
    }
}
