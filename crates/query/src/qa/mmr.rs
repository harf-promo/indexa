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

/// MMR score for one candidate chunk.
///
/// `mmr = λ * relevance - (1 - λ) * max_sim_to_selected`
///
/// `relevance` is the hit's raw RRF score **min-max normalized into `[0,1]` across the candidate
/// pool** (`rel_min`/`rel_span` computed once in [`apply_mmr`]). This is essential: raw RRF scores
/// are ~0.01–0.05 while the cosine `max_sim` diversity term is ~0.3–0.9, so without normalization
/// the diversity penalty dwarfs relevance ~20× and MMR effectively ignores how relevant a chunk is.
///
/// When `selected` is empty (no chunk chosen yet) the diversity penalty is zero, so the first pick
/// is always the highest-relevance chunk (its normalized relevance is the pool maximum, 1.0).
fn mmr_score(
    hit: &SearchHit,
    selected: &[&[f32]],
    lambda: f32,
    embeddings: &HashMap<i64, Vec<f32>>,
    rel_min: f32,
    rel_span: f32,
) -> f32 {
    // Normalize into [0,1]; when every candidate has the same score there's no relevance signal,
    // so treat all as maximally relevant (1.0) and let the diversity term order them.
    let rel = if rel_span > 0.0 {
        ((hit.rrf_score as f32 - rel_min) / rel_span).clamp(0.0, 1.0)
    } else {
        1.0
    };
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
    // Min-max normalization range for relevance, computed once over the whole pool (see `mmr_score`).
    let rel_min = candidates
        .iter()
        .map(|h| h.rrf_score as f32)
        .fold(f32::INFINITY, f32::min);
    let rel_max = candidates
        .iter()
        .map(|h| h.rrf_score as f32)
        .fold(f32::NEG_INFINITY, f32::max);
    let rel_span = rel_max - rel_min;

    let mut selected_vecs: Vec<&[f32]> = Vec::with_capacity(candidates.len());
    let mut result = Vec::with_capacity(candidates.len());

    // Greedy MMR selection — O(n²) in the number of candidates; at top_k=8..20
    // this is negligible.
    while !candidates.is_empty() {
        let best_idx = candidates
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let sa = mmr_score(a, &selected_vecs, lambda, embeddings, rel_min, rel_span);
                let sb = mmr_score(b, &selected_vecs, lambda, embeddings, rel_min, rel_span);
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
