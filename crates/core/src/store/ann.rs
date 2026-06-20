//! Optional in-memory HNSW approximate-nearest-neighbor index over chunk embeddings.
//!
//! Brute-force cosine (`store::search`) is fine to ~300K chunks; beyond that an ANN index
//! cuts dense-retrieval latency. This is **opt-in** (`[retrieval] ann`) and lives only in
//! long-lived processes (the web server caches one in `AppState`), built from the `chunks`
//! table on demand and rebuilt when the `(chunk_count, max_chunk_id)` watermark changes. It
//! is deliberately **not persisted**: `hnsw_rs` has no delete, and an on-disk index would
//! age against a changing table.
//!
//! Correctness rests on **stable chunk ids**: `chunks.id` is `AUTOINCREMENT` (see
//! `schema`), so an id is never reused after a re-`deep` deletes+reinserts a file. A node id
//! that's stale relative to the live table therefore resolves to either the *same* chunk's
//! content (still a valid neighbour) or *nothing* (dropped) — never a *different* file's
//! content. So a stale index can only cost recall, never mis-attribute a source. A
//! short-lived CLI `ask` never builds it (a one-shot brute-force scan beats building an
//! index used once); scoped queries, sub-threshold sizes, and ANN-off all fall back to
//! brute-force. ANN changes speed, not results.

use hnsw_rs::prelude::*;

/// HNSW build parameters — conservative, good for ~768-dim embeddings.
const MAX_NB_CONNECTION: usize = 24;
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;
/// Search-time exploration factor; larger = better recall, slower. Floored at the requested
/// k so a large fetch still explores enough candidates.
const EF_SEARCH: usize = 64;

/// An in-memory HNSW index mapping `chunk.id` → embedding, using true-cosine distance
/// (`DistCosine` is robust to un-normalized vectors, so Indexa's mix of L2-normalized and
/// raw embeddings is fine without pre-normalization).
pub struct AnnIndex {
    hnsw: Hnsw<'static, f32, DistCosine>,
    dim: usize,
    len: usize,
}

impl AnnIndex {
    /// Build from `(chunk_id, embedding)` pairs. Vectors whose length != `dim` are skipped
    /// (defensive against a mixed-dimension index after an embed-model change). The chunk id
    /// is stored as the HNSW data id so search results map straight back to chunk rows.
    pub fn build(items: &[(i64, Vec<f32>)], dim: usize) -> Self {
        let hnsw = Hnsw::<f32, DistCosine>::new(
            MAX_NB_CONNECTION,
            items.len().max(1),
            MAX_LAYER,
            EF_CONSTRUCTION,
            DistCosine {},
        );
        let mut len = 0;
        for (id, vec) in items {
            if vec.len() == dim {
                hnsw.insert((vec.as_slice(), *id as usize));
                len += 1;
            }
        }
        Self { hnsw, dim, len }
    }

    /// Number of vectors actually inserted.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `k` nearest chunk ids to `query` (closest first). Empty if the query dimension
    /// doesn't match the index.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<i64> {
        if query.len() != self.dim || self.len == 0 {
            return Vec::new();
        }
        let ef = EF_SEARCH.max(k);
        self.hnsw
            .search(query, k, ef)
            .into_iter()
            .map(|n| n.d_id as i64)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: [f32; 3]) -> Vec<f32> {
        v.to_vec()
    }

    #[test]
    fn ann_returns_nearest_neighbors() {
        // Three orthogonal-ish clusters; querying near one must return its id first.
        let items = vec![
            (10_i64, unit([1.0, 0.0, 0.0])),
            (20, unit([0.0, 1.0, 0.0])),
            (30, unit([0.0, 0.0, 1.0])),
            (11, unit([0.9, 0.1, 0.0])),
        ];
        let idx = AnnIndex::build(&items, 3);
        assert_eq!(idx.len(), 4);
        let near_x = idx.search(&[1.0, 0.05, 0.0], 2);
        assert!(
            near_x.contains(&10) || near_x.contains(&11),
            "expected the x-axis cluster, got {near_x:?}"
        );
        // The top hit for a near-pure-x query should be one of the x-axis vectors, not y/z.
        assert!(
            near_x.first() == Some(&10) || near_x.first() == Some(&11),
            "top hit should be an x-axis chunk, got {near_x:?}"
        );
    }

    #[test]
    fn ann_recall_matches_brute_force() {
        // Deterministic pseudo-random vectors (LCG); compare ANN top-k to brute-force cosine
        // top-k. HNSW is approximate, so we require high overlap, not identity.
        fn lcg(seed: &mut u64) -> f32 {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 1.0
        }
        let dim = 16;
        let n = 600;
        let mut seed = 0x1234_5678u64;
        let items: Vec<(i64, Vec<f32>)> = (0..n)
            .map(|i| (i as i64, (0..dim).map(|_| lcg(&mut seed)).collect()))
            .collect();
        let idx = AnnIndex::build(&items, dim);

        fn cosine(a: &[f32], b: &[f32]) -> f32 {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                dot / (na * nb)
            }
        }

        let k = 10;
        let mut total_overlap = 0usize;
        let queries = 12;
        for q in 0..queries {
            let query = &items[q * 50].1;
            // brute-force top-k ids
            let mut scored: Vec<(i64, f32)> = items
                .iter()
                .map(|(id, v)| (*id, cosine(query, v)))
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1)); // NaN-safe (no unwrap panic)
            let bf: std::collections::HashSet<i64> =
                scored.iter().take(k).map(|(id, _)| *id).collect();
            let ann: std::collections::HashSet<i64> = idx.search(query, k).into_iter().collect();
            total_overlap += bf.intersection(&ann).count();
        }
        // HNSW graph construction is randomized (hnsw_rs assigns node layers from an unseeded
        // RNG), so recall over this small synthetic index varies run-to-run and across
        // platforms — a tight 0.8 bar flaked intermittently on Windows CI. Averaging over more
        // queries and asserting ≥0.7 keeps the test a real guard against a broken index (which
        // would score ~k/n ≈ 0.02) while tolerating that build variance.
        let recall = total_overlap as f32 / (k * queries) as f32;
        assert!(
            recall >= 0.7,
            "ANN recall {recall:.2} too low vs brute-force"
        );
    }

    #[test]
    fn ann_skips_wrong_dim_and_handles_empty() {
        let items = vec![(1_i64, vec![1.0, 0.0, 0.0]), (2, vec![0.0, 1.0])]; // second is wrong-dim
        let idx = AnnIndex::build(&items, 3);
        assert_eq!(idx.len(), 1, "wrong-dimension vector must be skipped");
        assert!(
            idx.search(&[1.0, 0.0], 5).is_empty(),
            "wrong-dim query → empty"
        );
        assert!(AnnIndex::build(&[], 3)
            .search(&[1.0, 0.0, 0.0], 5)
            .is_empty());
    }
}
