//! Insights (v0.10): duplicate detection, stale-project detection, weekly diff.

use super::{search::blob_to_embedding, search::cosine_similarity, Store};
use anyhow::Result;
use rusqlite::params;

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
    pub fn find_near_duplicates(&self, threshold: f32) -> Result<Vec<DuplicateCluster>> {
        // Load all file summaries that have embeddings.
        let mut stmt = self.conn.prepare(
            "SELECT path, embedding FROM summaries WHERE kind='file' AND embedding IS NOT NULL",
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

        // Union-find clustering.
        let n = items.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut Vec<usize>, x: usize) -> usize {
            if parent[x] != x {
                parent[x] = find(parent, parent[x]);
            }
            parent[x]
        }

        // Pairwise similarity — O(n²); fine for typical index sizes (< 10K summary files).
        let mut pair_sims: Vec<(usize, usize, f32)> = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let sim = cosine_similarity(&items[i].1, &items[j].1);
                if sim >= threshold {
                    pair_sims.push((i, j, sim));
                    let pi = find(&mut parent, i);
                    let pj = find(&mut parent, j);
                    if pi != pj {
                        parent[pi] = pj;
                    }
                }
            }
        }

        // Group by root.
        let mut groups: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            groups.entry(root).or_default().push(i);
        }

        let mut clusters: Vec<DuplicateCluster> = groups
            .into_values()
            .filter(|g| g.len() >= 2)
            .map(|group| {
                let mut paths: Vec<String> = group.iter().map(|&i| items[i].0.clone()).collect();
                paths.sort();
                // Average similarity within group.
                let sims: Vec<f32> = pair_sims
                    .iter()
                    .filter(|(i, j, _)| group.contains(i) && group.contains(j))
                    .map(|(_, _, s)| *s)
                    .collect();
                let avg_sim = if sims.is_empty() {
                    threshold
                } else {
                    sims.iter().sum::<f32>() / sims.len() as f32
                };
                DuplicateCluster {
                    paths,
                    similarity: avg_sim,
                    exact: false,
                }
            })
            .collect();
        clusters.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(clusters)
    }

    // ── Stale detection ───────────────────────────────────────────────────────

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
