//! Jupyter notebook (`.ipynb`) parser: one logical chunk per cell.
//!
//! A notebook is JSON. Indexed as raw text it's noise (base64 image outputs, metadata);
//! here we extract each cell's *source* — tagging code cells with the kernel language so
//! they participate in code-aware retrieval — and skip outputs, which are usually large,
//! binary, or irrelevant to "what does this notebook do".

use crate::types::{chunk_words, Chunk, Extracted, Parser};
use anyhow::{anyhow, Result};
use std::path::Path;

pub struct IpynbParser;

impl Parser for IpynbParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(path.extension().and_then(|e| e.to_str()), Some("ipynb"))
    }

    /// `.ipynb` sniffs as `application/json`; it is dispatched by extension only so a
    /// plain `.json` file is never mistaken for a notebook.
    fn accepts_mime(&self, _mime: &str) -> bool {
        false
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let raw = std::fs::read_to_string(path)?;
        let json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow!("ipynb: invalid notebook JSON in {}: {e}", path.display()))?;

        // Kernel language for code cells (e.g. "python"); None if absent.
        let language = json
            .get("metadata")
            .and_then(|m| m.get("kernelspec"))
            .and_then(|k| k.get("language"))
            .and_then(|l| l.as_str())
            .map(str::to_owned);

        let cells = json.get("cells").and_then(|c| c.as_array());

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        if let Some(cells) = cells {
            for (i, cell) in cells.iter().enumerate() {
                let cell_type = cell.get("cell_type").and_then(|t| t.as_str()).unwrap_or("");
                let src = cell_source(cell);
                if src.trim().is_empty() {
                    continue;
                }
                let heading = format!("Cell {} [{cell_type}]", i + 1);
                let lang = if cell_type == "code" {
                    language.as_deref()
                } else {
                    None
                };
                chunk_words(path, &src, &heading, lang, 800, 100, &mut seq, &mut chunks);
            }
        }

        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!(
                    "Notebook: {} (no cell source)",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/x-ipynb+json".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// A cell's `source` is either a JSON string or an array of line-strings (nbformat v4).
fn cell_source(cell: &serde_json::Value) -> String {
    match cell.get("source") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(lines)) => {
            lines.iter().filter_map(|l| l.as_str()).collect::<String>()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipynb_extracts_cells_and_tags_code_language() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nb.ipynb");
        let nb = r##"{
          "cells": [
            {"cell_type":"markdown","source":["# Title\n","some intro prose here"]},
            {"cell_type":"code","source":"import os\nprint(os.getcwd())"},
            {"cell_type":"code","source":["x = 1\n","y = 2\n"]}
          ],
          "metadata": {"kernelspec": {"language": "python"}},
          "nbformat": 4
        }"##;
        std::fs::write(&p, nb).unwrap();
        let ex = IpynbParser.parse(&p).unwrap();
        assert!(ex.chunks.len() >= 3, "one chunk per non-empty cell");

        let code = ex
            .chunks
            .iter()
            .find(|c| c.text.contains("import os"))
            .expect("code cell extracted");
        assert_eq!(code.language.as_deref(), Some("python"));
        assert!(code.heading.contains("code"));

        let md = ex
            .chunks
            .iter()
            .find(|c| c.text.contains("some intro prose"))
            .expect("markdown cell extracted");
        assert_eq!(md.language, None, "markdown cells carry no language tag");
    }

    #[test]
    fn ipynb_invalid_json_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.ipynb");
        std::fs::write(&p, "this is not a notebook").unwrap();
        assert!(IpynbParser.parse(&p).is_err());
    }

    #[test]
    fn ipynb_dispatches_by_extension_not_json_mime() {
        assert!(IpynbParser.accepts_path(Path::new("/x/analysis.ipynb")));
        assert!(!IpynbParser.accepts_path(Path::new("/x/data.json")));
        assert!(!IpynbParser.accepts_mime("application/json"));
    }
}
