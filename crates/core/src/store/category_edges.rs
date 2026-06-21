//! Category (shared-classification) file edges for the knowledge-graph layer (Track 3, v0.71).
//!
//! A second opt-in overlay on the Map's call graph (alongside the v0.70 semantic layer): files the
//! user has classified into the SAME category (work / code / media / …) are grouped. To avoid an
//! O(n²) clique per category (every "code" file linked to every other), each category emits a
//! deterministic **star** from its representative (min path) to the rest — O(n) edges that still
//! visually cluster the category. Read-only, request-time-derived from the `classifications` table,
//! no schema, fail-open at the handler.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::params_from_iter;

use super::Store;

impl Store {
    /// Undirected "same category" edges among the displayed `nodes`: group the files by their
    /// confirmed/auto classification category (skipping uncategorized), and for each category with
    /// ≥2 members emit a star `(representative, member)` from the min-path representative. O(n)
    /// edges, deterministic (sorted). Returns **empty** when `nodes.len() > max_nodes` (cost guard).
    pub fn category_file_edges(
        &self,
        nodes: &[String],
        max_nodes: usize,
    ) -> Result<Vec<(String, String)>> {
        if nodes.len() < 2 || nodes.len() > max_nodes {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; nodes.len()].join(",");
        let sql = format!(
            "SELECT path, category FROM classifications \
             WHERE category != '' AND path IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(nodes.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;

        // category → member paths (one row per path; the IN clause restricts to the node set).
        let mut by_cat: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in rows {
            let (path, cat) = row?;
            by_cat.entry(cat).or_default().push(path);
        }

        let mut out: Vec<(String, String)> = Vec::new();
        for (_cat, mut members) in by_cat {
            members.sort();
            members.dedup();
            if members.len() < 2 {
                continue;
            }
            let rep = members[0].clone();
            for m in &members[1..] {
                out.push((rep.clone(), m.clone()));
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(classified: &[(&str, &str)]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("index.db")).unwrap();
        for (path, cat) in classified {
            store.confirm_classification(path, cat).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn groups_same_category_into_a_star_not_a_clique() {
        // Three "code" files → a 2-edge star from the min path, NOT a 3-edge clique.
        let (_d, s) = store_with(&[
            ("/p/b.rs", "code"),
            ("/p/a.rs", "code"),
            ("/p/c.rs", "code"),
            ("/p/doc.md", "work"), // lone "work" file → no edge
        ]);
        let nodes = vec![
            "/p/a.rs".into(),
            "/p/b.rs".into(),
            "/p/c.rs".into(),
            "/p/doc.md".into(),
        ];
        let edges = s.category_file_edges(&nodes, 100).unwrap();
        assert_eq!(
            edges.len(),
            2,
            "3-member category ⇒ star of 2 edges, not a clique"
        );
        assert_eq!(
            edges,
            vec![
                ("/p/a.rs".to_string(), "/p/b.rs".to_string()),
                ("/p/a.rs".to_string(), "/p/c.rs".to_string()),
            ],
            "star from the min-path representative, sorted"
        );
    }

    #[test]
    fn deterministic() {
        let (_d, s) = store_with(&[("/p/z.rs", "code"), ("/p/a.rs", "code")]);
        let nodes = vec!["/p/z.rs".into(), "/p/a.rs".into()];
        assert_eq!(
            s.category_file_edges(&nodes, 100).unwrap(),
            s.category_file_edges(&nodes, 100).unwrap()
        );
    }

    #[test]
    fn ignores_uncategorized_and_singletons() {
        let (_d, s) = store_with(&[("/p/a.rs", "code"), ("/p/b.md", "work")]);
        // Two files, two different categories ⇒ no same-category pair ⇒ no edges.
        let nodes = vec!["/p/a.rs".into(), "/p/b.md".into()];
        assert!(s.category_file_edges(&nodes, 100).unwrap().is_empty());
    }

    #[test]
    fn max_nodes_guard_returns_empty() {
        let (_d, s) = store_with(&[("/p/a.rs", "code"), ("/p/b.rs", "code")]);
        let nodes = vec!["/p/a.rs".into(), "/p/b.rs".into()];
        assert!(s.category_file_edges(&nodes, 1).unwrap().is_empty());
    }
}
