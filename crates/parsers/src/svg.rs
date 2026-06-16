//! SVG parser: extract the human-readable text from an SVG (it's XML/markup).
//!
//! Captures the text in `<text>`/`<tspan>`, `<title>`, `<desc>` and drops everything inside
//! a tag — so the path geometry (`d="…"` coordinates) and other attributes are never indexed.
//! Text inside `<style>`/`<script>` is skipped, and XML entities are decoded. There is no
//! rasterisation/OCR — text drawn as outlined vector paths (glyphs converted to shapes) is
//! not recovered. This indexes the *words in a diagram*, not its drawing instructions.

use crate::types::{chunk_words, Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct SvgParser;

impl Parser for SvgParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(path.extension().and_then(|e| e.to_str()), Some("svg"))
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "image/svg+xml"
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let raw = std::fs::read_to_string(path)?;
        let text = extract_svg_text(&raw);

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        chunk_words(path, &text, "", None, 800, 100, &mut seq, &mut chunks);
        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!(
                    "SVG image: {} (no embedded text)",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "image/svg+xml".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Strip SVG markup to its text content: drop everything inside `<…>` (so attributes such as
/// path `d="…"` geometry never leak), skip `<style>`/`<script>` bodies, separate every
/// element boundary with a space, and decode XML entities.
fn extract_svg_text(svg: &str) -> String {
    let mut result = String::with_capacity(svg.len());
    let mut in_tag = false;
    let mut skip_depth: u32 = 0; // inside <style>/<script>
    let mut tag_buf = String::new();
    for ch in svg.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                in_tag = false;
                let tag = tag_buf.trim().to_ascii_lowercase();
                let name = tag.split_whitespace().next().unwrap_or("");
                let is_close = name.starts_with('/');
                let bare = name.trim_start_matches('/');
                if matches!(bare, "style" | "script") {
                    if is_close {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !tag.ends_with('/') {
                        skip_depth += 1;
                    }
                }
                result.push(' '); // element boundary → word separator
            }
            _ if in_tag => tag_buf.push(ch),
            _ if skip_depth == 0 => result.push(ch),
            _ => {}
        }
    }
    let decoded = quick_xml::escape::unescape(&result)
        .map(|c| c.into_owned())
        .unwrap_or(result);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_extracts_title_desc_and_text_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("diagram.svg");
        std::fs::write(
            &p,
            r#"<svg xmlns="http://www.w3.org/2000/svg">
                 <title>System Diagram</title>
                 <desc>How the parts connect</desc>
                 <style>.box { fill: red; }</style>
                 <path d="M10 10 H 90 V 90 H 10 Z"/>
                 <text x="20" y="40">Auth service</text>
                 <text x="20" y="80">Database &amp; cache</text>
               </svg>"#,
        )
        .unwrap();
        let ex = SvgParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("System Diagram"), "{all}");
        assert!(all.contains("How the parts connect"), "{all}");
        assert!(all.contains("Auth service"), "{all}");
        assert!(all.contains("Database & cache"), "entity decoded: {all}");
        // Geometry and CSS are not indexed.
        assert!(!all.contains("M10"), "path geometry dropped: {all}");
        assert!(!all.contains("fill: red"), "style content skipped: {all}");
    }

    #[test]
    fn svg_without_text_yields_stub() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blank.svg");
        std::fs::write(
            &p,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>"#,
        )
        .unwrap();
        let ex = SvgParser.parse(&p).unwrap();
        assert_eq!(ex.chunks.len(), 1);
        assert!(ex.chunks[0].text.contains("no embedded text"));
    }

    #[test]
    fn svg_accepts_extension_and_mime() {
        assert!(SvgParser.accepts_path(Path::new("/x/logo.svg")));
        assert!(!SvgParser.accepts_path(Path::new("/x/logo.png")));
        assert!(SvgParser.accepts_mime("image/svg+xml"));
    }
}
