//! PDF text extraction via `pdf-extract` (pure Rust, text-layer PDFs only).
//!
//! Extracts text per-page and splits into ~800-word chunks with 100-word overlap.
//! Scanned / image-only PDFs will produce empty or near-empty output — OCR is an
//! opt-in enhancement (Marker or Tesseract, configurable in config.toml).

use crate::types::{Chunk, Extracted, Parser};
use anyhow::{anyhow, bail, Result};
use std::path::Path;

pub struct PdfParser;

impl Parser for PdfParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(path.extension().and_then(|e| e.to_str()), Some("pdf"))
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "application/pdf"
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let bytes = std::fs::read(path)?;

        // pdf_extract::extract_text_from_mem returns one big string.
        let text = pdf_extract::extract_text_from_mem(&bytes).unwrap_or_default();

        let mut chunks = Vec::new();

        if text.trim().is_empty() {
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
            // Try heading-aware sectioning first.
            let sections = split_by_headings(&text);
            let use_sections = sections.len() >= 2;

            if use_sections {
                let mut seq = 0usize;
                for (heading, body) in sections {
                    word_window_chunks(path, &body, &heading, &mut seq, &mut chunks);
                }
            } else {
                // Flat 800-word windows (original behaviour).
                let mut seq = 0usize;
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let full = format!("File: {name}\n{text}");
                    word_window_chunks(path, &full, "", &mut seq, &mut chunks);
                } else {
                    word_window_chunks(path, &text, "", &mut seq, &mut chunks);
                }
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/pdf".to_owned(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Split PDF text on detected heading lines.
/// Returns vec of (heading, body_text). Falls back to one section if none found.
fn split_by_headings(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_heading = String::new();
    let mut current_body = String::new();

    for line in text.lines() {
        if looks_like_heading(line) {
            if !current_body.trim().is_empty() {
                sections.push((current_heading.clone(), current_body.trim().to_owned()));
                current_body.clear();
            }
            current_heading = line.trim().to_owned();
        } else {
            current_body.push_str(line);
            current_body.push(' ');
        }
    }

    if !current_body.trim().is_empty() {
        sections.push((current_heading, current_body.trim().to_owned()));
    }

    sections
}

fn looks_like_heading(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    words.len() <= 8
        && !t.ends_with('.')
        && t.chars().next().is_some_and(|c| c.is_uppercase())
        && !t.contains(',')
}

fn word_window_chunks(
    path: &Path,
    text: &str,
    heading: &str,
    seq: &mut usize,
    chunks: &mut Vec<Chunk>,
) {
    crate::types::chunk_words(path, text, heading, None, 800, 100, seq, chunks);
}

/// OCR a scanned PDF (one with no text layer): rasterise each page to PNG with `pdftoppm`
/// (poppler) and run `tesseract` on it, concatenating the recognised text. Both are external
/// tools — if either is missing or fails this returns `Err`, so the caller falls open to the
/// (empty) text-layer result rather than crashing. Opt-in via `[parsers.pdf] backend = "ocr"`;
/// it runs in the indexing pipeline (`deep`/web), not the stateless parser.
pub fn ocr_pdf(path: &Path, tesseract_bin: &str, lang: Option<&str>) -> Result<String> {
    use std::process::Command;
    let dir = tempfile::tempdir()?;
    let prefix = dir.path().join("page");
    let status = Command::new("pdftoppm")
        .args(["-png", "-r", "200"])
        .arg(path)
        .arg(&prefix)
        .status()
        .map_err(|e| anyhow!("pdftoppm unavailable ({e}); install poppler for PDF OCR"))?;
    if !status.success() {
        bail!("pdftoppm failed to rasterise {}", path.display());
    }
    let mut pages: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
        .collect();
    pages.sort();
    let mut text = String::new();
    for page in pages {
        let mut cmd = Command::new(tesseract_bin);
        cmd.arg(&page).arg("stdout");
        if let Some(l) = lang {
            cmd.args(["-l", l]);
        }
        let out = cmd.output().map_err(|e| {
            anyhow!("{tesseract_bin} unavailable ({e}); install tesseract for PDF OCR")
        })?;
        if out.status.success() {
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            text.push('\n');
        }
    }
    Ok(text)
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

    #[test]
    fn ocr_pdf_missing_tools_is_graceful() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("scan.pdf");
        std::fs::write(&p, b"%PDF-1.4 minimal").unwrap();
        // With a bogus tesseract binary (and pdftoppm typically absent in CI), this must
        // return an Err rather than panic — the pipeline then falls open to the text layer.
        let _ = ocr_pdf(&p, "indexa-nonexistent-tesseract-binary", Some("eng"));
    }

    #[test]
    fn looks_like_heading_detects_short_title_case_lines() {
        assert!(looks_like_heading("Introduction"));
        assert!(looks_like_heading("Chapter One Background"));
        assert!(!looks_like_heading(
            "This is a full sentence with a period."
        ));
        assert!(!looks_like_heading("word, another word, more words here"));
        assert!(!looks_like_heading(""));
        assert!(!looks_like_heading("lowercase heading"));
    }

    #[test]
    fn split_by_headings_produces_sections() {
        let text =
            "Introduction\nSome intro body text here.\nBackground\nMore background content.\n";
        let sections = split_by_headings(text);
        assert!(sections.len() >= 2, "got {} sections", sections.len());
        assert_eq!(sections[0].0, "Introduction");
        assert!(sections[0].1.contains("intro body"));
    }
}
