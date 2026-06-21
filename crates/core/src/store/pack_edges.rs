//! Pack (shared Context-Pack membership) file edges for the knowledge-graph layer (Track 3, v0.72).
//!
//! The third opt-in Map overlay (after v0.70 "semantic" + v0.71 "category"): files the user grouped
//! into the same Context Pack are linked. Like the category layer, a pack emits a deterministic
//! **star** from its representative (min path) to the rest — O(n) edges, not an O(n²) clique — so a
//! large pack can't hairball the graph. Read-only, request-time-derived from `packs`/`pack_paths`,
//! no schema, fail-open at the handler. (Packs are explicit user curation, so these edges are exact,
//! not heuristic.)

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::params_from_iter;

use super::Store;

impl Store {
    /// Undirected "same pack" edges among the displayed `nodes`: group the files by Context-Pack
    /// membership, and for each pack with ≥2 members among the displayed set emit a star
    /// `(representative, member)` from the min-path representative. O(n) edges, deterministic
    /// (sorted). Returns **empty** when `nodes.len() > max_nodes` (cost guard). A file in several
    /// packs contributes an edge in each.
    pub fn pack_file_edges(
        &self,
        nodes: &[String],
        max_nodes: usize,
    ) -> Result<Vec<(String, String)>> {
        if nodes.len() < 2 || nodes.len() > max_nodes {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; nodes.len()].join(",");
        let sql = format!(
            "SELECT p.name, pp.path FROM packs p \
             JOIN pack_paths pp ON pp.pack_id = p.id \
             WHERE pp.path IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(nodes.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;

        // pack name → member paths (restricted to the node set by the IN clause).
        let mut by_pack: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in rows {
            let (pack, path) = row?;
            by_pack.entry(pack).or_default().push(path);
        }

        let mut out: Vec<(String, String)> = Vec::new();
        for (_pack, mut members) in by_pack {
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
        out.dedup(); // a file in two packs with the same rep won't double an edge
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(packs: &[(&str, &[&str])]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("index.db")).unwrap();
        for (name, paths) in packs {
            let id = store.create_pack(name, None).unwrap();
            let owned: Vec<String> = paths.iter().map(|s| (*s).to_owned()).collect();
            store.add_pack_paths(&id, &owned).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn groups_pack_members_into_a_star_not_a_clique() {
        let (_d, s) = store_with(&[("auth", &["/p/b.rs", "/p/a.rs", "/p/c.rs"])]);
        let nodes = vec!["/p/a.rs".into(), "/p/b.rs".into(), "/p/c.rs".into()];
        let edges = s.pack_file_edges(&nodes, 100).unwrap();
        assert_eq!(
            edges,
            vec![
                ("/p/a.rs".to_string(), "/p/b.rs".to_string()),
                ("/p/a.rs".to_string(), "/p/c.rs".to_string()),
            ],
            "3-member pack ⇒ star of 2 edges from the min-path rep, sorted"
        );
    }

    #[test]
    fn deterministic() {
        let (_d, s) = store_with(&[("p1", &["/p/z.rs", "/p/a.rs"])]);
        let nodes = vec!["/p/z.rs".into(), "/p/a.rs".into()];
        assert_eq!(
            s.pack_file_edges(&nodes, 100).unwrap(),
            s.pack_file_edges(&nodes, 100).unwrap()
        );
    }

    #[test]
    fn ignores_files_outside_the_node_set_and_singletons() {
        let (_d, s) = store_with(&[("p1", &["/p/a.rs", "/p/x.rs"])]);
        // Only /a is displayed ⇒ pack has a single displayed member ⇒ no edge.
        let nodes = vec!["/p/a.rs".into(), "/p/b.rs".into()];
        assert!(s.pack_file_edges(&nodes, 100).unwrap().is_empty());
    }

    #[test]
    fn max_nodes_guard_returns_empty() {
        let (_d, s) = store_with(&[("p1", &["/p/a.rs", "/p/b.rs"])]);
        let nodes = vec!["/p/a.rs".into(), "/p/b.rs".into()];
        assert!(s.pack_file_edges(&nodes, 1).unwrap().is_empty());
    }
}
