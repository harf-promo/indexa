//! Org-mode parser — heading-aware, handles code blocks, mirrors MarkdownParser pattern.

use crate::types::{Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct OrgParser {
    chunk_size: usize,
}

impl Default for OrgParser {
    fn default() -> Self {
        Self { chunk_size: 800 }
    }
}

impl Parser for OrgParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(path.extension().and_then(|e| e.to_str()), Some("org"))
    }

    fn accepts_mime(&self, _mime: &str) -> bool {
        // org-mode has no standard MIME type; rely on path-based detection
        false
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let raw = std::fs::read_to_string(path)?;
        let sections = collect_sections(&raw);

        let mut chunks = Vec::new();
        let mut seq = 0usize;

        for (heading, text, language) in sections {
            let words: Vec<&str> = text.split_whitespace().collect();
            if words.is_empty() {
                continue;
            }
            if words.len() <= self.chunk_size {
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: heading.clone(),
                    text,
                    language,
                });
                seq += 1;
            } else {
                let mut start = 0;
                loop {
                    let end = (start + self.chunk_size).min(words.len());
                    chunks.push(Chunk {
                        source: path.to_path_buf(),
                        seq,
                        heading: heading.clone(),
                        text: words[start..end].join(" "),
                        language: language.clone(),
                    });
                    seq += 1;
                    if end == words.len() {
                        break;
                    }
                    // saturating + min(1) so a chunk_size <= 100 can't underflow or stall.
                    start += self.chunk_size.saturating_sub(100).max(1);
                }
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/x-org".into(),
            chunks,
        })
    }
}

/// Walk the org source line by line, collecting (heading, text, language) triples.
fn collect_sections(raw: &str) -> Vec<(String, String, Option<String>)> {
    let mut sections: Vec<(String, String, Option<String>)> = Vec::new();
    let mut heading_stack: Vec<String> = Vec::new();
    let mut current_text = String::new();
    let mut src_lang: Option<String> = None;
    let mut src_content = String::new();

    for line in raw.lines() {
        // ── Source block boundaries ──────────────────────────────────────────
        if src_lang.is_some() {
            let lower = line.trim().to_lowercase();
            if lower == "#+end_src" {
                let lang = src_lang.take().unwrap();
                let body = src_content.trim().to_owned();
                if !body.is_empty() {
                    sections.push((heading_stack.join(" > "), body, Some(lang)));
                }
                src_content.clear();
            } else {
                src_content.push_str(line);
                src_content.push('\n');
            }
            continue;
        }

        let lower = line.trim().to_lowercase();
        if lower.starts_with("#+begin_src") {
            // Flush current prose section first
            flush_section(&heading_stack, &mut current_text, &mut sections);
            let lang = line.trim()[11..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            src_lang = Some(lang);
            continue;
        }

        // ── Heading lines ────────────────────────────────────────────────────
        if let Some(depth) = heading_depth(line) {
            flush_section(&heading_stack, &mut current_text, &mut sections);
            // Truncate stack to parent level
            heading_stack.truncate(depth.saturating_sub(1));
            let title = strip_org_markup(line[depth..].trim());
            heading_stack.push(title);
            continue;
        }

        // ── Skip directives / property drawers ───────────────────────────────
        let trimmed = line.trim();
        if trimmed.starts_with("#+")
            || trimmed == ":PROPERTIES:"
            || trimmed == ":END:"
            || (trimmed.starts_with(':') && trimmed.ends_with(':'))
        {
            continue;
        }

        // ── Normal content ───────────────────────────────────────────────────
        current_text.push_str(&strip_org_markup(line));
        current_text.push('\n');
    }

    // Flush trailing section
    flush_section(&heading_stack, &mut current_text, &mut sections);

    // Flush any unclosed src block
    if let Some(lang) = src_lang {
        let body = src_content.trim().to_owned();
        if !body.is_empty() {
            sections.push((heading_stack.join(" > "), body, Some(lang)));
        }
    }

    sections
}

fn flush_section(
    heading_stack: &[String],
    current_text: &mut String,
    sections: &mut Vec<(String, String, Option<String>)>,
) {
    let trimmed = current_text.trim().to_owned();
    if !trimmed.is_empty() {
        sections.push((heading_stack.join(" > "), trimmed, None));
    }
    current_text.clear();
}

/// Returns the heading depth (number of leading `*`) if line is an Org heading.
fn heading_depth(line: &str) -> Option<usize> {
    let stars = line.chars().take_while(|&c| c == '*').count();
    if stars == 0 {
        return None;
    }
    // Must be followed by a space (or end of line for degenerate case)
    let rest = &line[stars..];
    if rest.is_empty() || rest.starts_with(' ') {
        Some(stars)
    } else {
        None
    }
}

/// Strip common Org inline markup for cleaner indexing text.
fn strip_org_markup(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // [[link][description]] or [[link]]
        if chars[i] == '[' && i + 1 < chars.len() && chars[i + 1] == '[' {
            if let Some((text, end)) = parse_org_link(&chars, i) {
                result.push_str(&text);
                i = end + 1;
                continue;
            }
        }

        // Inline markup pairs: =code=, /italic/, *bold*, ~verbatim~, _underline_
        let marker = chars[i];
        if matches!(marker, '=' | '/' | '~') {
            // Only strip if at word boundary and marker repeats
            if let Some(end) = find_matching_marker(&chars, i, marker) {
                let inner: String = chars[i + 1..end].iter().collect();
                result.push_str(&inner);
                i = end + 1;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Find the closing marker on the same line. Returns the index of the closing marker.
fn find_matching_marker(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\n' {
            return None;
        }
        if chars[i] == marker && i > start + 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse `[[url][desc]]` or `[[url]]`, returning (text_to_use, end_index).
fn parse_org_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    // chars[start] == '[', chars[start+1] == '['
    let mut depth = 0i32;
    let mut i = start;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '[' && chars[i + 1] == '[' {
            depth += 1;
            i += 2;
        } else if i + 1 < chars.len() && chars[i] == ']' && chars[i + 1] == ']' {
            depth -= 1;
            if depth == 0 {
                let end = i + 1;
                // Content between outer [[ and ]]
                let content: String = chars[start + 2..i].iter().collect();
                // If there's a ][, use the text part (after ][)
                let text = if let Some(sep) = content.find("][") {
                    content[sep + 2..].to_string()
                } else {
                    content
                };
                return Some((text, end));
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(src: &str) -> Extracted {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.org");
        std::fs::write(&path, src).unwrap();
        OrgParser::default().parse(&path).unwrap()
    }

    #[test]
    fn org_parser_splits_on_headings() {
        let src = "* Introduction\nSome intro text.\n** Background\nBackground here.\n* Conclusion\nDone.\n";
        let ex = parse_str(src);
        assert!(ex.chunks.len() >= 2, "got {} chunks", ex.chunks.len());
        let headings: Vec<&str> = ex.chunks.iter().map(|c| c.heading.as_str()).collect();
        assert!(headings.iter().any(|h| h.contains("Introduction")));
        assert!(headings.iter().any(|h| h.contains("Conclusion")));
    }

    #[test]
    fn org_parser_code_block_becomes_chunk_with_language() {
        let src = "* Code section\n#+BEGIN_SRC rust\nfn main() {}\n#+END_SRC\n";
        let ex = parse_str(src);
        let code_chunk = ex.chunks.iter().find(|c| c.language.is_some()).unwrap();
        assert_eq!(code_chunk.language.as_deref(), Some("rust"));
        assert!(code_chunk.text.contains("fn main"));
    }

    #[test]
    fn org_parser_strips_link_markup() {
        let src = "* Links\n[[https://example.com][Click here]] for more info.\n";
        let ex = parse_str(src);
        let text = &ex.chunks[0].text;
        assert!(text.contains("Click here"), "{text}");
        assert!(!text.contains("[["), "{text}");
    }

    #[test]
    fn org_parser_handles_no_headings() {
        let src = "Just some plain text in an org file with no headings.\n";
        let ex = parse_str(src);
        assert_eq!(ex.chunks.len(), 1);
        assert!(ex.chunks[0].heading.is_empty());
    }

    #[test]
    fn org_parser_accepts_org_extension() {
        let p = OrgParser::default();
        assert!(p.accepts_path(Path::new("notes.org")));
        assert!(!p.accepts_path(Path::new("notes.md")));
    }
}
