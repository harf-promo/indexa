use std::path::PathBuf;

/// A semantic chunk extracted from a file — the unit of indexing.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// The file this chunk came from.
    pub source: PathBuf,
    /// Zero-based ordinal within the file (for stable ordering).
    pub seq: usize,
    /// Hierarchical heading breadcrumb, e.g. "Introduction > Background".
    /// Empty for unstructured chunks.
    pub heading: String,
    /// The text content to embed and store in FTS5.
    pub text: String,
    /// Optional language tag for code chunks ("rust", "python", etc.).
    pub language: Option<String>,
}

/// Output of a parser run on one file.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub source: PathBuf,
    pub mime: String,
    pub chunks: Vec<Chunk>,
}

/// Split `text` into overlapping fixed-size word windows, appending each as a [`Chunk`]
/// (sharing `source`/`heading`/`language`) and advancing `seq`. Shared by the plain-content
/// parsers (PDF, Office, EPUB, …) so the windowing logic lives in one place.
///
/// The stride is `(size - overlap)` clamped to at least 1, so neither a small `size` nor an
/// `overlap >= size` can underflow or stall the loop (the per-parser copies used a raw
/// `size - overlap`/`size - 100`, which could panic on subtraction overflow).
/// Empty/whitespace-only text appends nothing.
#[allow(clippy::too_many_arguments)] // a windowing primitive: source/heading/language/size/overlap + seq/sink
pub fn chunk_words(
    source: &std::path::Path,
    text: &str,
    heading: &str,
    language: Option<&str>,
    size: usize,
    overlap: usize,
    seq: &mut usize,
    chunks: &mut Vec<Chunk>,
) {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return;
    }
    let size = size.max(1);
    let stride = size.saturating_sub(overlap).max(1);
    let mut start = 0;
    loop {
        let end = (start + size).min(words.len());
        chunks.push(Chunk {
            source: source.to_path_buf(),
            seq: *seq,
            heading: heading.to_owned(),
            text: words[start..end].join(" "),
            language: language.map(|s| s.to_owned()),
        });
        *seq += 1;
        if end == words.len() {
            break;
        }
        start += stride;
    }
}

/// Trait implemented by every file parser.
pub trait Parser: Send + Sync {
    /// Returns true if this parser handles the given path.
    /// Default: delegates to `accepts_mime`. Override for extension-based detection.
    fn accepts_path(&self, path: &std::path::Path) -> bool {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        self.accepts_mime(&mime)
    }
    /// Returns true if this parser handles the given MIME type.
    fn accepts_mime(&self, mime: &str) -> bool;
    /// Parse the file at `path` and return extracted chunks.
    fn parse(&self, path: &std::path::Path) -> anyhow::Result<Extracted>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn chunk_words_windows_with_overlap() {
        let text = (0..10)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut seq = 0;
        let mut chunks = Vec::new();
        // size=4, stride=3 over 10 words → [0..4], [3..7], [6..10] = 3 chunks.
        chunk_words(
            Path::new("/a.txt"),
            &text,
            "h",
            Some("rust"),
            4,
            1,
            &mut seq,
            &mut chunks,
        );
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "w0 w1 w2 w3");
        assert_eq!(chunks[1].text, "w3 w4 w5 w6");
        assert_eq!(chunks[0].heading, "h");
        assert_eq!(chunks[0].language.as_deref(), Some("rust"));
        assert_eq!(seq, 3);
    }

    #[test]
    fn chunk_words_empty_text_appends_nothing() {
        let mut seq = 0;
        let mut chunks = Vec::new();
        chunk_words(
            Path::new("/a.txt"),
            "  \n  ",
            "",
            None,
            800,
            100,
            &mut seq,
            &mut chunks,
        );
        assert!(chunks.is_empty());
        assert_eq!(seq, 0);
    }

    #[test]
    fn chunk_words_overlap_ge_size_terminates() {
        // overlap >= size would make stride 0; it is clamped to 1 so the loop can't stall.
        let mut seq = 0;
        let mut chunks = Vec::new();
        chunk_words(
            Path::new("/a.txt"),
            "a b c d e",
            "",
            None,
            2,
            5,
            &mut seq,
            &mut chunks,
        );
        assert!(!chunks.is_empty());
        assert!(chunks.len() <= 10);
    }
}
