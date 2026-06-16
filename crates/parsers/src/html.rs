//! HTML parser: strip `<script>`/`<style>`, convert to Markdown (htmd), then chunk it
//! heading-aware via the shared Markdown sectioner — so an `<h1>`/`<h2>` structure becomes
//! the same breadcrumb headings a `.md` file would get. Dynamic/JS-rendered content is not
//! executed; tables are flattened to Markdown.

use crate::text::chunk_markdown;
use crate::types::{Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct HtmlParser {
    chunk_size: usize,
}

impl Default for HtmlParser {
    fn default() -> Self {
        Self { chunk_size: 800 }
    }
}

impl Parser for HtmlParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("html" | "htm" | "xhtml")
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "text/html" || mime == "application/xhtml+xml"
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[("html", Full), ("htm", Full), ("xhtml", Full)]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let html = std::fs::read_to_string(path)?;
        // htmd does not drop <script>/<style> content, so remove those blocks first (mirrors
        // the remote-source fetch path), then convert to Markdown. Fall back to the cleaned
        // HTML if conversion fails — the Markdown sectioner still extracts its text.
        let cleaned = strip_blocks(&strip_blocks(&html, "script"), "style");
        let markdown = htmd::convert(&cleaned).unwrap_or(cleaned);

        let mut chunks = chunk_markdown(path, &markdown, self.chunk_size);
        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!(
                    "HTML: {} (no extractable text)",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/html".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Remove every `<tag …>…</tag>` block (case-insensitive), keeping the text around them, so a
/// `<script>`/`<style>` body's JS/CSS can't leak into the converted Markdown. An unterminated
/// block drops the remainder.
fn strip_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while pos < html.len() {
        match lower[pos..].find(&open) {
            Some(rel) => {
                let start = pos + rel;
                out.push_str(&html[pos..start]); // keep text before the block
                match lower[start..].find(&close) {
                    Some(crel) => pos = start + crel + close.len(),
                    None => pos = html.len(), // unterminated → drop the rest
                }
            }
            None => {
                out.push_str(&html[pos..]);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_extracts_headings_and_text_dropping_script_style() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("page.html");
        std::fs::write(
            &p,
            r#"<!doctype html><html><head><style>.x{color:red}</style>
               <script>var leak = 1;</script></head>
               <body><h1>Welcome</h1><p>The auth flow lives in the gateway.</p>
               <h2>Details</h2><p>Sessions are minted per request.</p></body></html>"#,
        )
        .unwrap();
        let ex = HtmlParser::default().parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("auth flow"), "{all}");
        assert!(all.contains("Sessions are minted"), "{all}");
        assert!(!all.contains("color:red"), "style leaked: {all}");
        assert!(!all.contains("var leak"), "script leaked: {all}");
        assert!(
            ex.chunks.iter().any(|c| c.heading.contains("Welcome")),
            "headings preserved: {:?}",
            ex.chunks.iter().map(|c| &c.heading).collect::<Vec<_>>()
        );
    }

    #[test]
    fn html_accepts_extensions_and_mime() {
        let p = HtmlParser::default();
        assert!(p.accepts_path(Path::new("/x/index.html")));
        assert!(p.accepts_path(Path::new("/x/page.htm")));
        assert!(!p.accepts_path(Path::new("/x/page.md")));
        assert!(p.accepts_mime("text/html"));
    }

    #[test]
    fn strip_blocks_removes_named_block() {
        let out = strip_blocks("a<script>x=1</script>b", "script");
        assert_eq!(out, "ab");
    }
}
