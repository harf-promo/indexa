//! Insights (v0.10): duplicate detection, stale-project detection, weekly diff.

use super::{search::blob_to_embedding, search::cosine_similarity, Store};
use anyhow::Result;
use rusqlite::params;
use std::collections::{HashMap, HashSet};

/// A cluster of near-duplicate files (similarity ≥ threshold).
#[derive(Debug, Clone)]
pub struct DuplicateCluster {
    /// Paths in the cluster (sorted alphabetically).
    pub paths: Vec<String>,
    /// Average pair-wise similarity within the cluster.
    pub similarity: f32,
    /// Whether this is an exact duplicate (matching source_hash) or near-duplicate.
    pub exact: bool,
}

/// A large indexed file (bloat detection).
#[derive(Debug, Clone)]
pub struct LargestEntry {
    pub path: String,
    pub size: u64,
}

/// One language's share of indexed content, by chunk count.
#[derive(Debug, Clone)]
pub struct LanguageStat {
    pub language: String,
    pub chunks: u64,
}

/// A stale entry (not modified within the given threshold).
#[derive(Debug, Clone)]
pub struct StaleEntry {
    pub path: String,
    pub kind: String,
    pub modified_s: Option<i64>,
    pub days_since_modified: i64,
}

/// Summary of index changes over a time window.
#[derive(Debug, Clone)]
pub struct WeeklyDiff {
    /// Paths added to the index within the window (first_indexed_at >= since).
    pub added: Vec<String>,
    /// Paths modified on disk within the window but already in the index.
    pub modified: Vec<String>,
    /// Total counts.
    pub added_count: usize,
    pub modified_count: usize,
}

impl Store {
    // ── Duplicate detection ───────────────────────────────────────────────────

    /// Find files with identical content fingerprints (exact duplicates).
    /// Groups `summaries.source_hash` values that appear on more than one path.
    pub fn find_exact_duplicates(&self) -> Result<Vec<DuplicateCluster>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_hash, GROUP_CONCAT(path, '|||') AS paths
             FROM summaries
             WHERE source_hash != '' AND kind = 'file'
             GROUP BY source_hash
             HAVING COUNT(*) > 1
             ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let paths_str: String = r.get(1)?;
            Ok(paths_str)
        })?;
        let mut clusters = Vec::new();
        for row in rows {
            let paths_str = row?;
            let mut paths: Vec<String> = paths_str.split("|||").map(|s| s.to_owned()).collect();
            paths.sort();
            clusters.push(DuplicateCluster {
                paths,
                similarity: 1.0,
                exact: true,
            });
        }
        Ok(clusters)
    }

    /// Find near-duplicate files by summary embedding cosine similarity.
    /// Returns clusters of paths where pairwise similarity ≥ `threshold`.
    /// Operates on summary embeddings (file-level, not chunk-level).
    ///
    /// Strategy (v0.24 — the old silent 5,000-file cap is gone; whole-disk
    /// indexes are scanned in full):
    /// - n ≤ [`NEAR_DUP_EXACT_MAX`]: exhaustive O(n²) scan — exact and instant.
    /// - n > [`NEAR_DUP_EXACT_MAX`]: random-hyperplane LSH candidate generation
    ///   with exact cosine verification. **Approximate**: pairs whose similarity
    ///   sits near the threshold can be missed (recall caveat documented on
    ///   [`near_dup_clusters_lsh`]); verified pairs are never false positives.
    ///   Exact duplicates are unaffected — `find_exact_duplicates` groups by
    ///   hash in SQL with no cap.
    ///
    /// Memory is O(n·dim): one streamed pass over (path, embedding) rows; the
    /// LSH path adds O(n) signatures and a candidate set bounded linear in n
    /// by the bucket cap.
    pub fn find_near_duplicates(&self, threshold: f32) -> Result<Vec<DuplicateCluster>> {
        // Stream every embedded file summary — no row cap. ORDER BY path gives
        // a deterministic item order, which both paths rely on for reproducible
        // output (bucket insertion order, float-sum order).
        let mut stmt = self.conn.prepare(
            "SELECT path, embedding FROM summaries
             WHERE kind='file' AND embedding IS NOT NULL
             ORDER BY path",
        )?;
        let items: Vec<(String, Vec<f32>)> = stmt
            .query_map([], |r| {
                let path: String = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                Ok((path, blob))
            })?
            .filter_map(|row| {
                row.ok().and_then(|(path, blob)| {
                    if blob.len() % 4 == 0 && !blob.is_empty() {
                        Some((path, blob_to_embedding(&blob)))
                    } else {
                        None
                    }
                })
            })
            .collect();

        if items.len() < 2 {
            return Ok(Vec::new());
        }

        let clusters = if items.len() <= NEAR_DUP_EXACT_MAX {
            near_dup_clusters_exact(&items, threshold)
        } else {
            near_dup_clusters_lsh(&items, threshold)
        };
        Ok(clusters)
    }

    // ── Stale detection ───────────────────────────────────────────────────────

    /// The `limit` largest indexed files by on-disk size (bloat detection).
    pub fn find_largest(&self, limit: usize) -> Result<Vec<LargestEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size FROM entries
             WHERE kind = 'file'
             ORDER BY size DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(LargestEntry {
                path: r.get(0)?,
                size: r.get::<_, i64>(1)? as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Language breakdown of indexed content by chunk count, most-chunks first.
    /// Only code chunks carry a language tag; untagged chunks are excluded.
    pub fn language_breakdown(&self) -> Result<Vec<LanguageStat>> {
        let mut stmt = self.conn.prepare(
            "SELECT language, COUNT(*) AS n FROM chunks
             WHERE language IS NOT NULL AND language != ''
             GROUP BY language
             ORDER BY n DESC, language ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(LanguageStat {
                language: r.get(0)?,
                chunks: r.get::<_, i64>(1)? as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Return entries not modified on disk for more than `days` days.
    pub fn find_stale_entries(&self, days: i64) -> Result<Vec<StaleEntry>> {
        let cutoff = days * 86_400;
        let mut stmt = self.conn.prepare(
            "SELECT path, kind, modified_s,
                    (unixepoch() - COALESCE(modified_s, 0)) / 86400 AS days_age
             FROM entries
             WHERE kind = 'dir'
               AND (modified_s IS NULL OR modified_s < (unixepoch() - ?1))
             ORDER BY days_age DESC
             LIMIT 500",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok(StaleEntry {
                path: r.get(0)?,
                kind: r.get(1)?,
                modified_s: r.get(2)?,
                days_since_modified: r.get::<_, i64>(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Weekly diff ───────────────────────────────────────────────────────────

    /// Return what changed in the index since `since_secs` (Unix timestamp):
    /// - `added`: paths where `first_indexed_at >= since_secs` (newly discovered)
    /// - `modified`: paths where `modified_s >= since_secs` and already indexed before
    pub fn weekly_diff(&self, since_secs: i64) -> Result<WeeklyDiff> {
        let mut added_stmt = self.conn.prepare(
            "SELECT path FROM entries
             WHERE first_indexed_at IS NOT NULL AND first_indexed_at >= ?1
             ORDER BY first_indexed_at DESC LIMIT 500",
        )?;
        let added: Vec<String> = added_stmt
            .query_map(params![since_secs], |r| r.get(0))?
            .collect::<Result<_, _>>()?;

        let mut modified_stmt = self.conn.prepare(
            "SELECT path FROM entries
             WHERE modified_s >= ?1
               AND (first_indexed_at IS NULL OR first_indexed_at < ?1)
             ORDER BY modified_s DESC LIMIT 500",
        )?;
        let modified: Vec<String> = modified_stmt
            .query_map(params![since_secs], |r| r.get(0))?
            .collect::<Result<_, _>>()?;

        let added_count = added.len();
        let modified_count = modified.len();
        Ok(WeeklyDiff {
            added,
            modified,
            added_count,
            modified_count,
        })
    }
}

// ── Near-duplicate clustering internals ──────────────────────────────────────

/// At or below this many embedded file summaries the exhaustive O(n²) scan
/// runs (≤ ~2M cosine ops — instant, and exact). Above it, LSH takes over.
pub(super) const NEAR_DUP_EXACT_MAX: usize = 2000;

/// LSH geometry: `LSH_BANDS` bands × `LSH_BAND_BITS` sign bits each =
/// `LSH_BANDS * LSH_BAND_BITS` hyperplanes total. 12 bits/band → 4,096
/// possible buckets per band (4 bits would give only 16 — every bucket would
/// overflow past a few thousand files); 8 bands → ~93% recall for pairs at
/// sim = 0.95, →100% as sim → 1.
const LSH_BANDS: usize = 8;
const LSH_BAND_BITS: usize = 12;

/// Fixed SplitMix64 seed for hyperplane generation. A compile-time constant
/// (no time/OS entropy) so the same DB always yields the same clusters.
const LSH_SEED: u64 = 0x1DE2_A5EE_D000_0024;

/// Per-(band, bucket) membership cap. Members within a bucket pair all-vs-all
/// (≤ C(200,2) ≈ 19,900 cosine checks per bucket); an item arriving at a full
/// bucket is instead chained to the bucket's first member only (1 candidate),
/// so giant near-identical crowds still merge transitively through that hub.
/// Bound: each item joins ≤ LSH_BANDS buckets of ≤ LSH_MAX_BUCKET members →
/// total candidates ≤ n · LSH_BANDS · LSH_MAX_BUCKET / 2 — linear in n.
const LSH_MAX_BUCKET: usize = 200;

/// SplitMix64 (Steele et al., public-domain algorithm). Used only to generate
/// the fixed LSH hyperplanes — deterministic, no `rand` dependency.
pub(super) struct SplitMix64(pub(super) u64);

impl SplitMix64 {
    pub(super) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform f32 in [-1, 1). Cube-uniform (not Gaussian) directions are fine
    /// here: bucketing only generates candidates; exact cosine verifies them.
    pub(super) fn next_unit(&mut self) -> f32 {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
    }
}

/// Union-find root lookup with path compression.
fn uf_find(parent: &mut [usize], x: usize) -> usize {
    if parent[x] != x {
        parent[x] = uf_find(parent, parent[x]);
    }
    parent[x]
}

/// Build sorted [`DuplicateCluster`]s from union-find state plus the verified
/// pair similarities. `similarity` is the average over the verified pairs in
/// each cluster — for the LSH path that is the subset of in-cluster pairs that
/// collided in some band, not necessarily all of them.
fn clusters_from_pairs(
    items: &[(String, Vec<f32>)],
    parent: &mut [usize],
    pair_sims: &[(usize, usize, f32)],
    threshold: f32,
) -> Vec<DuplicateCluster> {
    // Accumulate per-root pair sums in pair_sims order (deterministic float
    // addition), then group members by root.
    let mut sums: HashMap<usize, (f32, usize)> = HashMap::new();
    for &(i, _, s) in pair_sims {
        let root = uf_find(parent, i);
        let e = sums.entry(root).or_insert((0.0, 0));
        e.0 += s;
        e.1 += 1;
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..items.len() {
        let root = uf_find(parent, i);
        groups.entry(root).or_default().push(i);
    }

    let mut clusters: Vec<DuplicateCluster> = groups
        .into_iter()
        .filter(|(_, g)| g.len() >= 2)
        .map(|(root, group)| {
            let mut paths: Vec<String> = group.iter().map(|&i| items[i].0.clone()).collect();
            paths.sort();
            let similarity = match sums.get(&root) {
                Some(&(sum, count)) if count > 0 => sum / count as f32,
                _ => threshold,
            };
            DuplicateCluster {
                paths,
                similarity,
                exact: false,
            }
        })
        .collect();
    // Path tiebreaker: HashMap iteration order is run-dependent, so similarity
    // ties must not decide the output order.
    clusters.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.paths.cmp(&b.paths))
    });
    clusters
}

/// Exhaustive O(n²) near-duplicate clustering — exact. Used for n ≤
/// [`NEAR_DUP_EXACT_MAX`], where it is instant.
pub(super) fn near_dup_clusters_exact(
    items: &[(String, Vec<f32>)],
    threshold: f32,
) -> Vec<DuplicateCluster> {
    let n = items.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut pair_sims: Vec<(usize, usize, f32)> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let sim = cosine_similarity(&items[i].1, &items[j].1);
            if sim >= threshold {
                pair_sims.push((i, j, sim));
                let pi = uf_find(&mut parent, i);
                let pj = uf_find(&mut parent, j);
                if pi != pj {
                    parent[pi] = pj;
                }
            }
        }
    }
    clusters_from_pairs(items, &mut parent, &pair_sims, threshold)
}

/// Approximate near-duplicate clustering via random-hyperplane LSH.
///
/// Each embedding gets `LSH_BANDS × LSH_BAND_BITS` sign bits against
/// hyperplanes seeded from [`LSH_SEED`] — same DB, same clusters, every run.
/// Two items become a candidate pair when any band's bits match exactly;
/// every candidate is verified with exact cosine against `threshold`, so
/// reported pairs are never false positives.
///
/// **Honesty caveat — this is approximate.** A pair only surfaces if it
/// collides in at least one band: P[band match] = (1 − θ/π)^LSH_BAND_BITS,
/// so pairs whose similarity sits near the threshold CAN BE MISSED (~93%
/// recall at sim = 0.95 with the current 8×12 geometry, →100% as sim → 1).
/// Exact-hash duplicates are caught separately by `find_exact_duplicates`.
///
/// Cost bound: bucket membership is capped at [`LSH_MAX_BUCKET`]; overflow
/// items chain to the bucket's first member (hub) instead, keeping crowds
/// connected while the candidate count stays ≤ n·LSH_BANDS·LSH_MAX_BUCKET/2.
pub(super) fn near_dup_clusters_lsh(
    items: &[(String, Vec<f32>)],
    threshold: f32,
) -> Vec<DuplicateCluster> {
    let n = items.len();
    let dim = items.iter().map(|(_, e)| e.len()).max().unwrap_or(0);
    if dim == 0 {
        return Vec::new();
    }

    // Hyperplanes: (LSH_BANDS · LSH_BAND_BITS) × dim, from the fixed seed.
    let mut rng = SplitMix64(LSH_SEED);
    let planes: Vec<Vec<f32>> = (0..LSH_BANDS * LSH_BAND_BITS)
        .map(|_| (0..dim).map(|_| rng.next_unit()).collect())
        .collect();

    // Bucket items per band. Shorter embeddings (mixed embed models — rare)
    // dot over their own length via zip; verification is exact regardless.
    let mut buckets: HashMap<(u8, u16), Vec<u32>> = HashMap::new();
    let mut candidates: HashSet<(u32, u32)> = HashSet::new();
    for (idx, (_, emb)) in items.iter().enumerate() {
        for band in 0..LSH_BANDS {
            let mut key: u16 = 0;
            for bit in 0..LSH_BAND_BITS {
                let plane = &planes[band * LSH_BAND_BITS + bit];
                let dot: f32 = emb.iter().zip(plane).map(|(x, y)| x * y).sum();
                key = (key << 1) | u16::from(dot >= 0.0);
            }
            let bucket = buckets.entry((band as u8, key)).or_default();
            if bucket.len() < LSH_MAX_BUCKET {
                bucket.push(idx as u32);
            } else {
                // Full bucket: chain to the hub so union-find still absorbs
                // the crowd at 1 candidate per overflow item (see LSH_MAX_BUCKET).
                candidates.insert((bucket[0], idx as u32));
            }
        }
    }
    for members in buckets.values() {
        for (a, &i) in members.iter().enumerate() {
            for &j in &members[a + 1..] {
                candidates.insert((i, j));
            }
        }
    }
    // Sort the deduped candidates: HashSet iteration order is run-dependent,
    // and pair order affects float-sum rounding in the cluster averages.
    let mut cand: Vec<(u32, u32)> = candidates.into_iter().collect();
    cand.sort_unstable();

    let mut parent: Vec<usize> = (0..n).collect();
    let mut pair_sims: Vec<(usize, usize, f32)> = Vec::new();
    for (i, j) in cand {
        let (i, j) = (i as usize, j as usize);
        let sim = cosine_similarity(&items[i].1, &items[j].1);
        if sim >= threshold {
            pair_sims.push((i, j, sim));
            let pi = uf_find(&mut parent, i);
            let pj = uf_find(&mut parent, j);
            if pi != pj {
                parent[pi] = pj;
            }
        }
    }
    clusters_from_pairs(items, &mut parent, &pair_sims, threshold)
}
