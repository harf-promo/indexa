use crate::types::{Chunk, Extracted, Parser};
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
            if !chunk_text.trim().is_empty() {
                chunks.push(Chunk {
                    source: source.to_path_buf(),
                    seq,
                    heading: String::new(),
                    text: chunk_text,
                    language: None,
                });
                seq += 1;
            }
            if end == words.len() {
                break;
            }
            // step forward with overlap
            start += self.chunk_size.saturating_sub(self.overlap);
        }
        chunks
    }
}

impl Parser for TextParser {
    fn accepts_mime(&self, mime: &str) -> bool {
        mime.starts_with("text/plain")
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Extracted> {
        let text = std::fs::read_to_string(path)?;
        let chunks = self.fixed_chunks(path, &text);
        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/plain".into(),
            chunks,
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

    fn parse(&self, path: &Path) -> anyhow::Result<Extracted> {
        let raw = std::fs::read_to_string(path)?;

        let opts =
            Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES | Options::ENABLE_STRIKETHROUGH;
        let parser = MdParser::new_ext(&raw, opts);

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
                        sections
                            .push((current_heading.join(" > "), current_text.trim().to_owned()));
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
            if words.len() <= self.chunk_size {
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: heading.clone(),
                    text,
                    language: None,
                });
                seq += 1;
            } else {
                let mut start = 0;
                while start < words.len() {
                    let end = (start + self.chunk_size).min(words.len());
                    chunks.push(Chunk {
                        source: path.to_path_buf(),
                        seq,
                        heading: heading.clone(),
                        text: words[start..end].join(" "),
                        language: None,
                    });
                    seq += 1;
                    if end == words.len() {
                        break;
                    }
                    start += self.chunk_size.saturating_sub(100); // 100-word overlap
                }
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "text/markdown".into(),
            chunks,
        })
    }
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
}
