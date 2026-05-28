//! PDF text extraction via `pdf-extract` (pure Rust, text-layer PDFs only).
//!
//! Extracts text per-page and splits into ~800-word chunks with 100-word overlap.
//! Scanned / image-only PDFs will produce empty or near-empty output — OCR is an
//! opt-in enhancement (Marker or Tesseract, configurable in config.toml).

use crate::types::{Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct PdfParser;

impl Parser for PdfParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pdf")
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "application/pdf"
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let bytes = std::fs::read(path)?;

        // pdf_extract::extract_text_from_mem returns one big string.
        let text = pdf_extract::extract_text_from_mem(&bytes).unwrap_or_default();

        let mut parts = Vec::new();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            parts.push(format!("File: {name}"));
        }
        if !text.trim().is_empty() {
            parts.push(text.clone());
        }

        let full_text = parts.join("\n");

        // Split into ~800-word chunks with 100-word overlap.
        let words: Vec<&str> = full_text.split_whitespace().collect();
        let mut chunks = Vec::new();
        let mut seq = 0usize;

        if words.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!(
                    "PDF: {} (no extractable text — may be scanned)",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ),
                language: None,
            });
        } else {
            let size = 800usize;
            let overlap = 100usize;
            let mut start = 0;
            loop {
                let end = (start + size).min(words.len());
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: String::new(),
                    text: words[start..end].join(" "),
                    language: None,
                });
                seq += 1;
                if end == words.len() {
                    break;
                }
                start += size - overlap;
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/pdf".to_owned(),
            chunks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_parser_accepts_pdf_extension() {
        let p = PdfParser;
        assert!(p.accepts_path(Path::new("doc.pdf")));
        assert!(!p.accepts_path(Path::new("doc.docx")));
        assert!(!p.accepts_path(Path::new("doc.txt")));
    }

    #[test]
    fn pdf_parser_handles_corrupt_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.pdf");
        std::fs::write(&p, b"not a real pdf").unwrap();
        let parser = PdfParser;
        // Should not panic — either extracts empty or falls back gracefully.
        let extracted = parser.parse(&p).unwrap();
        assert_eq!(extracted.chunks.len(), 1);
        assert!(
            extracted.chunks[0].text.contains("bad.pdf")
                || extracted.chunks[0].text.contains("PDF")
        );
    }
}
