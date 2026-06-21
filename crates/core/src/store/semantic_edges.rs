//! Semantic (meaning-similarity) file edges for the knowledge-graph layer (Track 3, v0.70).
//!
//! The Map's call graph links files that *call* each other; this adds a second, opt-in edge type
//! that links files with *similar content* (a topic relationship the call graph can't see). Edges
//! are **derived at request time** from `chunks.embedding` — no schema, no persistence (re-derivable,
//! like classifications). Read-only; fails open at the handler.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::params;

use super::search::{blob_to_embedding, cosine_similarity, like_prefix, STUB_EXCLUDE_SQL};
use super::Store;

impl Store {
    /// Undirected semantic edges between FILES in `nodes` (under `scope`): for each file, build a
    /// centroid from its chunk embeddings, then emit `(min_path, max_path, similarity)` for every
    /// pair whose centroid cosine ≥ `threshold`. Output is deterministic (canonical `(min,max)`
    /// ordering, sorted).
    ///
    /// O(n²) over the node set, so it returns **empty** when `nodes.len() > max_nodes` (the cost
    /// guard — the handler keeps `max_nodes` well under the graph's node cap). Dimension mismatches
    /// and missing embeddings are skipped, never errored.
    pub fn semantic_file_edges(
        &self,
        scope: &str,
        nodes: &[String],
        threshold: f32,
        max_nodes: usize,
    ) -> Result<Vec<(String, String, f32)>> {
        if nodes.len() < 2 || nodes.len() > max_nodes {
            return Ok(Vec::new());
        }
        let want: HashSet<&str> = nodes.iter().map(String::as_str).collect();

        // One scoped, stub-excluded scan; accumulate a sum-vector + count per file (its centroid).
        let sql = format!(
            "SELECT entry_path, embedding FROM chunks \
             WHERE embedding IS NOT NULL AND entry_path LIKE ?1 ESCAPE '\\'{STUB_EXCLUDE_SQL}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let pattern = like_prefix(scope);
        let mut rows = stmt.query(params![pattern])?;

        let mut sums: HashMap<String, (Vec<f32>, usize)> = HashMap::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            if !want.contains(path.as_str()) {
                continue;
            }
            let blob: Vec<u8> = row.get(1)?;
            if !blob.len().is_multiple_of(4) {
                continue;
            }
            let v = blob_to_embedding(&blob);
            let entry = sums.entry(path).or_insert_with(|| (vec![0.0; v.len()], 0));
            if entry.0.len() != v.len() {
                continue; // dimension mismatch within a file — skip the odd chunk
            }
            for (acc, x) in entry.0.iter_mut().zip(&v) {
                *acc += x;
            }
            entry.1 += 1;
        }

        // Mean → centroid per file.
        let centroids: Vec<(String, Vec<f32>)> = sums
            .into_iter()
            .filter(|(_, (_, n))| *n > 0)
            .map(|(p, (mut s, n))| {
                let inv = 1.0 / n as f32;
                for x in &mut s {
                    *x *= inv;
                }
                (p, s)
            })
            .collect();

        // Pairwise cosine over centroids (n² but bounded by `max_nodes`).
        let mut out: Vec<(String, String, f32)> = Vec::new();
        for i in 0..centroids.len() {
            for j in (i + 1)..centroids.len() {
                let (a, va) = (&centroids[i].0, &centroids[i].1);
                let (b, vb) = (&centroids[j].0, &centroids[j].1);
                if va.len() != vb.len() {
                    continue;
                }
                let sim = cosine_similarity(va, vb);
                if sim >= threshold {
                    if a <= b {
                        out.push((a.clone(), b.clone(), sim));
                    } else {
                        out.push((b.clone(), a.clone(), sim));
                    }
                }
            }
        }
        out.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ChunkRecord;

    fn store_with(chunks: &[(&str, &[f32])]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        let recs: Vec<ChunkRecord> = chunks
            .iter()
            .enumerate()
            .map(|(i, (path, emb))| ChunkRecord {
                entry_path: (*path).to_owned(),
                seq: i,
                heading: String::new(),
                text: format!("body of chunk {i} with enough words to not be a stub at all"),
                language: None,
                embedding: Some(emb.to_vec()),
                embed_model: Some("test".to_owned()),
                content_hash: None,
            })
            .collect();
        store.upsert_chunks(&recs).unwrap();
        (dir, store)
    }

    #[test]
    fn emits_edges_between_similar_files_only() {
        // /a and /b are near-parallel; /c is orthogonal.
        let (_d, s) = store_with(&[
            ("/p/a.rs", &[1.0, 0.0]),
            ("/p/b.rs", &[0.99, 0.01]),
            ("/p/c.rs", &[0.0, 1.0]),
        ]);
        let nodes = vec!["/p/a.rs".into(), "/p/b.rs".into(), "/p/c.rs".into()];
        let edges = s.semantic_file_edges("/p", &nodes, 0.8, 100).unwrap();
        assert_eq!(edges.len(), 1, "only the a–b pair is above threshold");
        assert_eq!(
            (edges[0].0.as_str(), edges[0].1.as_str()),
            ("/p/a.rs", "/p/b.rs")
        );
    }

    #[test]
    fn deterministic_canonical_ordering() {
        let (_d, s) = store_with(&[("/p/z.rs", &[1.0, 0.0]), ("/p/a.rs", &[1.0, 0.0])]);
        let nodes = vec!["/p/z.rs".into(), "/p/a.rs".into()];
        let e1 = s.semantic_file_edges("/p", &nodes, 0.5, 100).unwrap();
        let e2 = s.semantic_file_edges("/p", &nodes, 0.5, 100).unwrap();
        assert_eq!(e1, e2, "same inputs ⇒ same output");
        assert_eq!(e1[0].0, "/p/a.rs", "edge endpoints are (min, max) by path");
    }

    #[test]
    fn max_nodes_guard_returns_empty() {
        let (_d, s) = store_with(&[("/p/a.rs", &[1.0, 0.0]), ("/p/b.rs", &[1.0, 0.0])]);
        let nodes = vec!["/p/a.rs".into(), "/p/b.rs".into()];
        // node count (2) exceeds the cap (1) ⇒ empty (the O(n²) cost guard).
        assert!(s
            .semantic_file_edges("/p", &nodes, 0.5, 1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn restricts_to_the_requested_node_set() {
        let (_d, s) = store_with(&[("/p/a.rs", &[1.0, 0.0]), ("/p/b.rs", &[1.0, 0.0])]);
        // Only /a is in the node set ⇒ no pair ⇒ no edges (a file out of the set is ignored).
        let nodes = vec!["/p/a.rs".into()];
        assert!(s
            .semantic_file_edges("/p", &nodes, 0.5, 100)
            .unwrap()
            .is_empty());
    }
}
