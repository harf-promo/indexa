//! Community detection over the displayed knowledge graph (v0.72) via **Louvain local-moving**
//! (one level of greedy modularity maximization).
//!
//! Powers the Map's opt-in "Communities" view: group files into communities, surface each
//! community's hub (highest-centrality file), and highlight cross-community "bridge" edges
//! (surprising connections). Runs over the *displayed* call graph (post-scope/focus), so
//! communities are relative to what's shown — labelled "approximate" in the UI, like PageRank.
//!
//! Louvain (not plain label propagation): LPA floods a single label across a bridge into one
//! "monster community" on small graphs; modularity maximization correctly keeps two cliques joined
//! by a weak link as two communities. Pure, dependency-free (no `petgraph`), and **deterministic**:
//! fixed node order (the caller's path-sorted indices), each node seeded in its own community,
//! greedy moves to the neighbouring community of highest modularity gain (neighbours visited in
//! sorted-id order, **strict**-improvement only so ties keep the current community), a fixed
//! `MAX_ITER` cap, and a canonical dense relabel. Fail-open: empty on an empty/over-cap graph.

use std::collections::{BTreeMap, HashMap};

/// Max local-moving passes. Convergence is fast on graphs of the displayed size; the cap
/// guarantees termination regardless.
const MAX_ITER: usize = 50;

/// O(passes × edges) cost guard: above this node count, skip detection (return empty). The
/// displayed graph is capped at `limit ≤ 2000` (default 300), so this only guards a pathological
/// unfocused whole-disk load.
const MAX_NODES: usize = 1500;

/// Assign each of `n` nodes a community label over the undirected `edges` (index pairs, each in
/// `0..n`; multiplicity = weight). Returns a `Vec<usize>` of length `n`, **canonicalized** to dense
/// labels `0..k` in first-seen order (so the frontend tints stably). Deterministic for a given
/// `(n, edges)`. Returns **empty** when `n == 0` or `n > MAX_NODES` (the caller treats empty as
/// "no communities" and renders the plain graph). Total — never panics (out-of-range / self-loop
/// edges are skipped defensively).
pub fn detect_communities(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    if n == 0 || n > MAX_NODES {
        return Vec::new();
    }

    // Undirected adjacency + weighted degree; skip self-loops + out-of-range indices defensively.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut deg: Vec<f64> = vec![0.0; n];
    let mut m = 0.0f64; // total undirected edge weight
    for &(a, b) in edges {
        if a >= n || b >= n || a == b {
            continue;
        }
        adj[a].push(b);
        adj[b].push(a);
        deg[a] += 1.0;
        deg[b] += 1.0;
        m += 1.0;
    }
    if m == 0.0 {
        // No edges ⇒ every node is its own community (already dense 0..n).
        return (0..n).collect();
    }
    let two_m = 2.0 * m;

    // Each node starts in its own community; `sigma_tot[c]` = sum of degrees of c's members.
    let mut comm: Vec<usize> = (0..n).collect();
    let mut sigma_tot: Vec<f64> = deg.clone();

    for _ in 0..MAX_ITER {
        let mut changed = false;
        for i in 0..n {
            if adj[i].is_empty() {
                continue;
            }
            let ci = comm[i];
            // Tentatively remove i from its community.
            sigma_tot[ci] -= deg[i];
            // Edge weight from i into each neighbouring community (sorted ids ⇒ deterministic).
            let mut k_in: BTreeMap<usize, f64> = BTreeMap::new();
            for &nb in &adj[i] {
                *k_in.entry(comm[nb]).or_insert(0.0) += 1.0;
            }
            // Baseline = gain of putting i back in its own (now-removed) community.
            let stay = *k_in.get(&ci).unwrap_or(&0.0) - sigma_tot[ci] * deg[i] / two_m;
            let mut best_c = ci;
            let mut best_gain = stay;
            for (&c, &kic) in &k_in {
                let gain = kic - sigma_tot[c] * deg[i] / two_m;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_c = c;
                }
            }
            sigma_tot[best_c] += deg[i];
            comm[i] = best_c;
            if best_c != ci {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Canonicalize: relabel by first-seen order → dense `0..k`.
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    comm.iter()
        .map(|&c| {
            *remap.entry(c).or_insert_with(|| {
                let v = next;
                next += 1;
                v
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num_communities(labels: &[usize]) -> usize {
        labels.iter().copied().max().map(|m| m + 1).unwrap_or(0)
    }

    #[test]
    fn empty_and_over_cap_return_empty() {
        assert!(detect_communities(0, &[]).is_empty());
        assert!(detect_communities(MAX_NODES + 1, &[]).is_empty());
    }

    #[test]
    fn edgeless_nodes_are_each_their_own_community() {
        let labels = detect_communities(3, &[]);
        assert_eq!(labels.len(), 3);
        assert_eq!(num_communities(&labels), 3);
    }

    #[test]
    fn fully_connected_collapses_to_one_community() {
        // Triangle.
        let labels = detect_communities(3, &[(0, 1), (1, 2), (0, 2)]);
        assert_eq!(num_communities(&labels), 1);
    }

    #[test]
    fn two_cliques_one_bridge_split_into_two_communities() {
        // Triangles {0,1,2} and {3,4,5} joined by a single bridge (2,3).
        let edges = [
            (0, 1),
            (1, 2),
            (0, 2),
            (3, 4),
            (4, 5),
            (3, 5),
            (2, 3), // the bridge / "surprising connection"
        ];
        let labels = detect_communities(6, &edges);
        assert_eq!(num_communities(&labels), 2, "two cliques ⇒ two communities");
        assert_eq!(labels[0], labels[1]);
        assert_eq!(labels[1], labels[2]);
        assert_eq!(labels[3], labels[4]);
        assert_eq!(labels[4], labels[5]);
        assert_ne!(labels[2], labels[3], "the bridge crosses communities");
    }

    #[test]
    fn deterministic_and_canonical() {
        let edges = [(0, 1), (1, 2), (3, 4), (4, 5), (2, 3)];
        let a = detect_communities(6, &edges);
        let b = detect_communities(6, &edges);
        assert_eq!(a, b, "same input ⇒ same labels");
        // Canonical: labels are dense 0..k (no gaps).
        let k = num_communities(&a);
        for c in 0..k {
            assert!(a.contains(&c), "label {c} must be present (dense 0..k)");
        }
        // First node is always community 0 (first-seen canonicalization).
        assert_eq!(a[0], 0);
    }

    #[test]
    fn self_loops_and_out_of_range_edges_are_ignored() {
        // (0,0) self-loop + (1,9) out of range ⇒ skipped, no panic.
        let labels = detect_communities(3, &[(0, 0), (1, 9), (1, 2)]);
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[1], labels[2], "the valid edge still groups 1 and 2");
    }
}
