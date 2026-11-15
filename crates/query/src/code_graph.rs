//! Shared code-graph persistence for the live-update paths (`indexa watch` and the web
//! server's watch sessions).
//!
//! `deep` writes a file's `edges` and `symbols` rows via `Store::replace_file_code_graph`,
//! then skips that file on later runs while its chunks are current. A live watcher that
//! re-chunks a file but leaves the graph alone therefore strands the pre-edit symbol line
//! ranges and imports until the file changes again with watch off. Anything that rewrites a
//! file's chunks must refresh its graph in the same pass, through this one record builder.

use anyhow::Result;
use indexa_core::store::{EdgeRecord, Store, SymbolRecord};
use indexa_parsers::types::Edge;

/// Turn one file's extracted code-graph edges into the `edges` and `symbols` rows stored for
/// it. `defines` edges that carry a line range also become symbol rows.
pub fn code_graph_records(path: &str, edges: &[Edge]) -> (Vec<EdgeRecord>, Vec<SymbolRecord>) {
    let edge_records = edges
        .iter()
        .map(|e| EdgeRecord {
            from_path: path.to_owned(),
            kind: e.kind.to_owned(),
            to_ref: e.to.clone(),
        })
        .collect();
    let symbol_records = edges
        .iter()
        .filter(|e| e.kind == "defines")
        .filter_map(|e| {
            let (start, end) = e.line_range?;
            Some(SymbolRecord {
                path: path.to_owned(),
                name: e.to.clone(),
                kind: e.symbol_kind.unwrap_or("other").to_owned(),
                start_line: start as i64,
                end_line: end as i64,
            })
        })
        .collect();
    (edge_records, symbol_records)
}

/// Replace `path`'s code-graph rows with the ones built from `edges`. An empty `edges` still
/// clears the file's old rows (a successful parse with no symbols is meaningful), exactly as
/// `deep` does.
pub fn refresh_file_code_graph(store: &mut Store, path: &str, edges: &[Edge]) -> Result<()> {
    let (edge_records, symbol_records) = code_graph_records(path, edges);
    store.replace_file_code_graph(path, &edge_records, &symbol_records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn edge(kind: &'static str, to: &str, range: Option<(usize, usize)>) -> Edge {
        Edge {
            from: PathBuf::from("/r/a.rs"),
            kind,
            to: to.to_owned(),
            symbol_kind: range.map(|_| "fn"),
            line_range: range,
        }
    }

    #[test]
    fn defines_edges_with_a_range_become_symbols_and_every_edge_is_kept() {
        let edges = vec![
            edge("imports", "std::fmt", None),
            edge("defines", "run", Some((3, 9))),
            edge("calls", "helper", None),
        ];
        let (e, s) = code_graph_records("/r/a.rs", &edges);
        assert_eq!(e.len(), 3);
        assert_eq!(s.len(), 1);
        assert_eq!(
            (s[0].name.as_str(), s[0].start_line, s[0].end_line),
            ("run", 3, 9)
        );
    }

    #[test]
    fn refresh_replaces_old_rows_and_an_empty_result_clears_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("idx.db")).unwrap();
        refresh_file_code_graph(
            &mut store,
            "/r/a.rs",
            &[edge("defines", "run", Some((3, 9)))],
        )
        .unwrap();
        refresh_file_code_graph(
            &mut store,
            "/r/a.rs",
            &[edge("defines", "run", Some((10, 20)))],
        )
        .unwrap();
        let syms = store.symbols_in_file("/r/a.rs").unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!((syms[0].start_line, syms[0].end_line), (10, 20));
        refresh_file_code_graph(&mut store, "/r/a.rs", &[]).unwrap();
        assert!(store.symbols_in_file("/r/a.rs").unwrap().is_empty());
    }
}
