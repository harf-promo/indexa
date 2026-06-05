//! Weighted PageRank over the file-to-file call graph (v0.20 centrality).
//!
//! Rank flows along directed edges `from → to` (caller → callee), so a file
//! that is *called by* many — or by other important — files accumulates rank.
//! That surfaces hub/library files as the most central, which is what the Map
//! graph view and `indexa graph` highlight.
//!
//! Pure, dependency-free (no `petgraph`): a fixed-point power iteration with the
//! standard 0.85 damping factor, dangling-node mass redistribution, and an L1
//! convergence early-out. Scores sum to ~1.0. The graph it runs on is the
//! *displayed* (post-cap) subgraph from [`super::Store::code_graph`], so on a
//! truncated graph centrality is relative to what's shown — documented in
//! `docs/methodology.md` and labelled honestly in the UI.

const DAMPING: f64 = 0.85;
const MAX_ITER: usize = 100;
const CONVERGENCE_EPS: f64 = 1e-9;

/// Compute a PageRank score per node for a directed, edge-weighted graph.
///
/// `n` is the node count; `edges` are `(from_idx, to_idx, weight)` triples with
/// indices in `0..n` and strictly positive weights. Returns a `Vec<f64>` of
/// length `n` (score per node index) that sums to ~1.0. An empty graph yields
/// an empty vector; an edgeless graph yields the uniform distribution `1/n`.
pub(super) fn pagerank(n: usize, edges: &[(usize, usize, f64)]) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }

    // Sum of outgoing edge weights per node (0 ⇒ dangling).
    let mut out_weight = vec![0.0_f64; n];
    for &(from, _to, w) in edges {
        out_weight[from] += w;
    }

    let inv_n = 1.0 / n as f64;
    let teleport = (1.0 - DAMPING) * inv_n;
    let mut rank = vec![inv_n; n];

    for _ in 0..MAX_ITER {
        // Mass stranded on dangling nodes is redistributed uniformly, preserving
        // the total so the distribution keeps summing to 1.
        let dangling_mass: f64 = (0..n)
            .filter(|&u| out_weight[u] == 0.0)
            .map(|u| rank[u])
            .sum();
        let base = teleport + DAMPING * dangling_mass * inv_n;

        let mut next = vec![base; n];
        for &(from, to, w) in edges {
            if out_weight[from] > 0.0 {
                next[to] += DAMPING * rank[from] * (w / out_weight[from]);
            }
        }

        let delta: f64 = rank.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < CONVERGENCE_EPS {
            break;
        }
    }

    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_sum(v: &[f64]) -> f64 {
        v.iter().sum()
    }

    #[test]
    fn empty_graph_is_empty() {
        assert!(pagerank(0, &[]).is_empty());
    }

    #[test]
    fn edgeless_graph_is_uniform() {
        let r = pagerank(3, &[]);
        assert_eq!(r.len(), 3);
        for x in &r {
            assert!((x - 1.0 / 3.0).abs() < 1e-9, "got {x}");
        }
        assert!((approx_sum(&r) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn hub_called_by_many_scores_highest() {
        // Nodes 0,1,2 each call node 3 (the hub). Rank should concentrate on 3.
        let edges = [(0, 3, 1.0), (1, 3, 1.0), (2, 3, 1.0)];
        let r = pagerank(4, &edges);
        assert!(
            (approx_sum(&r) - 1.0).abs() < 1e-6,
            "sum={}",
            approx_sum(&r)
        );
        let hub = r[3];
        for leaf in &r[0..3] {
            assert!(hub > *leaf, "hub {hub} should beat leaf {leaf}");
        }
    }

    #[test]
    fn heavier_edge_directs_more_rank() {
        // Source 0 splits its rank between 1 and 2; the heavier edge wins.
        let edges = [(0, 1, 3.0), (0, 2, 1.0)];
        let r = pagerank(3, &edges);
        assert!(
            r[1] > r[2],
            "heavy target {} should beat light {}",
            r[1],
            r[2]
        );
    }

    #[test]
    fn converges_on_a_cycle() {
        // A 3-cycle is symmetric ⇒ all ranks equal, and it must not oscillate.
        let edges = [(0, 1, 1.0), (1, 2, 1.0), (2, 0, 1.0)];
        let r = pagerank(3, &edges);
        assert!((approx_sum(&r) - 1.0).abs() < 1e-6);
        assert!((r[0] - r[1]).abs() < 1e-6 && (r[1] - r[2]).abs() < 1e-6);
    }
}
