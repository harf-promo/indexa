use crate::types::{split_char_budget, Chunk, Extracted, Parser, MAX_CHUNK_CHARS};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser as MdParser, Tag, TagEnd};
use std::path::Path;

pub struct TextParser {
    chunk_size: usize,
    overlap: usize,
}

impl Default for TextParser {
    fn default() -> Self {
        Self {
            chunk_size: 800,
            overlap: 100,
        }
    }
}

impl TextParser {
    pub fn new(chunk_size: usize, overlap: usize) -> Self {
        Self {
            chunk_size,
            overlap,
        }
    }

    fn fixed_chunks(&self, source: &Path, text: &str) -> Vec<Chunk> {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }

        let mut chunks = Vec::new();
        let mut start = 0;
        let mut seq = 0;

        while start < words.len() {
            let end = (start + self.chunk_size).min(words.len());
            let chunk_text = words[start..end].join(" ");
            // Cap each window at MAX_CHUNK_CHARS so a file with few whitespace-separated
            // "words" (minified CSS/HTML, long lines) can't emit a chunk that overflows
            // the embedder's context window. Splits oversized windows into pieces.
            for piece in split_char_budget(&chunk_text, MAX_CHUNK_CHARS) {
                if !piece.trim().is_empty() {
                    chunks.push(Chunk {
                        source: source.to_path_buf(),
                        seq,
                        heading: String::new(),
                        text: piece.to_owned(),
                        language: None,
                    });
                    seq += 1;
                }
            }
            if end == words.len() {
                break;
            }
            // step forward with overlap; .max(1) so a config with overlap >= chunk_size can't
            // produce a zero stride and spin the loop forever (matches org.rs's guard).
            start += self.chunk_size.saturating_sub(self.overlap).max(1);
        }
        chunks
    }
}

impl Parser for TextParser {
    fn accepts_mime(&self, mime: &str) -> bool {
        mime.starts_with("text/plain")
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[
            ("txt", Full),
            ("log", Full),
            ("conf", Full),
            ("ini", Full),
            ("yaml", Full),
            ("yml", Full),
            ("json", Full),
            ("toml", Full),
            ("xml", Full),
            ("css", Full),
        ]
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Extracted> {
        let text = std::fs::read_to_string(path)?;
        let chunks = self.fixed_chunks(path, &text);
        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/plain".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────

pub struct MarkdownParser {
    chunk_size: usize,
}

impl Default for MarkdownParser {
    fn default() -> Self {
        Self { chunk_size: 800 }
    }
}

impl MarkdownParser {
    pub fn new(chunk_size: usize) -> Self {
        Self { chunk_size }
    }
}

impl Parser for MarkdownParser {
    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "text/markdown" || mime == "text/x-markdown"
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[("md", Full), ("mdx", Full)]
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Extracted> {
        let raw = std::fs::read_to_string(path)?;
        let (frontmatter, body) = split_frontmatter(&raw);

        let mut chunks = chunk_markdown(path, &body, self.chunk_size);

        // Lift frontmatter metadata (title/tags/date/…) into a leading, searchable chunk.
        if let Some(meta) = frontmatter {
            chunks.insert(
                0,
                Chunk {
                    source: path.to_path_buf(),
                    seq: 0,
                    heading: "frontmatter".into(),
                    text: meta,
                    language: None,
                },
            );
            for (i, c) in chunks.iter_mut().enumerate() {
                c.seq = i;
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/markdown".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Section a Markdown string into heading-breadcrumbed chunks (≤ `chunk_size` words each,
/// 100-word overlap on long sections, each char-capped). Shared by [`MarkdownParser`] and the
/// HTML parser, which converts HTML → Markdown first.
pub(crate) fn chunk_markdown(path: &Path, markdown: &str, chunk_size: usize) -> Vec<Chunk> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES | Options::ENABLE_STRIKETHROUGH;
    let parser = MdParser::new_ext(markdown, opts);

    let mut sections: Vec<(String, String)> = Vec::new(); // (heading_breadcrumb, text)
    let mut current_heading: Vec<String> = Vec::new();
    let mut current_text = String::new();
    let mut in_heading = false;
    let mut heading_buf = String::new();

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // flush current section
                if !current_text.trim().is_empty() {
                    sections.push((current_heading.join(" > "), current_text.trim().to_owned()));
                    current_text.clear();
                }
                in_heading = true;
                heading_buf.clear();
                // truncate breadcrumb to current level depth
                let depth = match level {
                    HeadingLevel::H1 => 0,
                    HeadingLevel::H2 => 1,
                    HeadingLevel::H3 => 2,
                    _ => 3,
                };
                current_heading.truncate(depth);
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                if !heading_buf.is_empty() {
                    current_heading.push(heading_buf.trim().to_owned());
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if in_heading {
                    heading_buf.push_str(&t);
                } else {
                    current_text.push_str(&t);
                    current_text.push(' ');
                }
            }
            Event::SoftBreak | Event::HardBreak if !in_heading => {
                current_text.push('\n');
            }
            _ => {}
        }
    }
    // flush last section
    if !current_text.trim().is_empty() {
        sections.push((current_heading.join(" > "), current_text.trim().to_owned()));
    }

    // Split any section that exceeds chunk_size words into smaller chunks.
    let mut chunks = Vec::new();
    let mut seq = 0usize;
    for (heading, text) in sections {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() <= chunk_size {
            // Even a short-word section can be char-huge; cap it.
            for piece in split_char_budget(&text, MAX_CHUNK_CHARS) {
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: heading.clone(),
                    text: piece.to_owned(),
                    language: None,
                });
                seq += 1;
            }
        } else {
            let mut start = 0;
            while start < words.len() {
                let end = (start + chunk_size).min(words.len());
                let window = words[start..end].join(" ");
                for piece in split_char_budget(&window, MAX_CHUNK_CHARS) {
                    chunks.push(Chunk {
                        source: path.to_path_buf(),
                        seq,
                        heading: heading.clone(),
                        text: piece.to_owned(),
                        language: None,
                    });
                    seq += 1;
                }
                if end == words.len() {
                    break;
                }
                // saturating + .max(1) so a chunk_size <= 100 can't yield a zero stride and
                // spin the loop forever (matches org.rs's guard).
                start += chunk_size.saturating_sub(100).max(1); // 100-word overlap
            }
        }
    }
    chunks
}

/// Split a leading YAML frontmatter block (`---` … `---`) from the markdown body.
///
/// Returns `(Some("key: val · …"), body)` when a *closed* frontmatter block is present,
/// lifting the common `title`/`tags`/`date`/`description`/`author` keys so they become
/// searchable; arbitrary nested YAML is ignored. Returns `(None, raw)` when there is no
/// frontmatter (or it is never closed), leaving a leading `---` horizontal rule intact.
fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    if raw.lines().next().map(str::trim_end) != Some("---") {
        return (None, raw.to_owned());
    }
    let mut fields: Vec<String> = Vec::new();
    let mut closed = false;
    let mut consumed = 1; // the opening "---"
    for line in raw.lines().skip(1) {
        consumed += 1;
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "title" | "tags" | "date" | "description" | "author"
            ) {
                let val = v.trim().trim_matches(|c| c == '"' || c == '\'');
                if !val.is_empty() {
                    fields.push(format!("{key}: {val}"));
                }
            }
        }
    }
    if !closed {
        return (None, raw.to_owned());
    }
    let body = raw.lines().skip(consumed).collect::<Vec<_>>().join("\n");
    let meta = if fields.is_empty() {
        None
    } else {
        Some(fields.join(" · "))
    };
    (meta, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_parser_chunks_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "one two three four five six seven eight nine ten").unwrap();
        let parser = TextParser::new(5, 2);
        let extracted = parser.parse(&p).unwrap();
        assert!(!extracted.chunks.is_empty());
        assert_eq!(extracted.chunks[0].text, "one two three four five");
    }

    #[test]
    fn markdown_parser_splits_on_headings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("test.md");
        std::fs::write(
            &p,
            "# Introduction\n\nSome intro text.\n\n## Background\n\nBackground content here.",
        )
        .unwrap();
        let parser = MarkdownParser::default();
        let extracted = parser.parse(&p).unwrap();
        assert_eq!(extracted.chunks.len(), 2);
        assert!(extracted.chunks[0].heading.contains("Introduction"));
        assert!(extracted.chunks[1].heading.contains("Background"));
    }

    #[test]
    fn markdown_parser_handles_no_headings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("flat.md");
        std::fs::write(&p, "Just a paragraph with no headings at all.").unwrap();
        let parser = MarkdownParser::default();
        let extracted = parser.parse(&p).unwrap();
        assert_eq!(extracted.chunks.len(), 1);
        assert!(extracted.chunks[0].heading.is_empty());
    }

    #[test]
    fn markdown_lifts_frontmatter_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("post.md");
        std::fs::write(
            &p,
            "---\ntitle: My Post\ntags: rust, indexing\ndate: 2026-06-16\n---\n\n# Body\n\nThe actual content.",
        )
        .unwrap();
        let ex = MarkdownParser::default().parse(&p).unwrap();
        assert_eq!(ex.chunks[0].heading, "frontmatter");
        assert_eq!(ex.chunks[0].seq, 0);
        assert!(
            ex.chunks[0].text.contains("title: My Post"),
            "{}",
            ex.chunks[0].text
        );
        assert!(ex.chunks[0].text.contains("tags: rust, indexing"));
        let body: String = ex
            .chunks
            .iter()
            .skip(1)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(body.contains("actual content"), "{body}");
        assert!(!body.contains("---"), "fence leaked into body: {body}");
    }

    #[test]
    fn markdown_without_frontmatter_is_unaffected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plain.md");
        std::fs::write(&p, "# Title\n\nNo frontmatter here.").unwrap();
        let ex = MarkdownParser::default().parse(&p).unwrap();
        assert!(ex.chunks.iter().all(|c| c.heading != "frontmatter"));
        assert!(ex.chunks[0].heading.contains("Title"));
    }

    #[test]
    fn text_parser_terminates_when_overlap_ge_chunk_size() {
        // Degenerate config: overlap (5) >= chunk_size (3). Without the `.max(1)` stride guard
        // the fixed-window loop would advance by 0 and spin forever. Must terminate with a
        // bounded number of chunks over a multi-window input.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("degenerate.txt");
        let text = (0..30)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(&p, &text).unwrap();
        let ex = TextParser::new(3, 5).parse(&p).unwrap();
        assert!(!ex.chunks.is_empty());
        assert!(ex.chunks.len() <= 30, "got {} chunks", ex.chunks.len());
    }

    #[test]
    fn markdown_parser_terminates_when_chunk_size_le_overlap() {
        // The per-section word-splitter uses a fixed 100-word overlap; a chunk_size <= 100 would
        // give a zero stride without the `.max(1)` guard. Must terminate over a >chunk_size body.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("degenerate.md");
        let body = (0..40)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(&p, format!("# H\n\n{body}")).unwrap();
        let ex = MarkdownParser::new(2).parse(&p).unwrap();
        assert!(!ex.chunks.is_empty());
        assert!(ex.chunks.len() <= 60, "got {} chunks", ex.chunks.len());
    }
}
