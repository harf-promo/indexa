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

/// Hard ceiling on a single chunk's length in **chars**, independent of the word
/// window. `nomic-embed-text`'s context is ~2048 tokens; ~4000 chars ≈ ~1000 tokens,
/// leaving generous headroom. Without this cap a file with few whitespace-separated
/// "words" — minified CSS/HTML, or long single lines — emits one giant chunk that
/// makes Ollama 500 with "the input length exceeds the context length".
pub const MAX_CHUNK_CHARS: usize = 4000;

/// Split `s` into consecutive pieces, each at most `max_chars` chars, breaking on a
/// UTF-8 boundary (preferring the last ASCII space in the window for a cleaner cut).
/// `floor_char_boundary` is nightly-only, so the cut from `char_indices().nth(..)` is
/// already a valid boundary by construction. Returns borrowed slices of `s`.
pub(crate) fn split_char_budget(s: &str, max_chars: usize) -> Vec<&str> {
    let max_chars = max_chars.max(1);
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        // Byte offset just past the `max_chars`-th char, or end of `rest`.
        let cut = match rest.char_indices().nth(max_chars) {
            Some((i, _)) => i,
            None => {
                out.push(rest);
                break;
            }
        };
        // Prefer to break at the last ASCII space in the first half-or-more of the
        // window (a space is 1 byte, so `w + 1` is always a valid boundary).
        let piece_end = match rest[..cut].rfind(' ') {
            Some(w) if w + 1 >= max_chars / 2 => w + 1,
            _ => cut,
        };
        out.push(&rest[..piece_end]);
        rest = &rest[piece_end..];
    }
    out
}

/// Split `text` into overlapping fixed-size word windows, appending each as a [`Chunk`]
/// (sharing `source`/`heading`/`language`) and advancing `seq`. Shared by the plain-content
/// parsers (PDF, Office, EPUB, …) so the windowing logic lives in one place.
///
/// The stride is `(size - overlap)` clamped to at least 1, so neither a small `size` nor an
/// `overlap >= size` can underflow or stall the loop (the per-parser copies used a raw
/// `size - overlap`/`size - 100`, which could panic on subtraction overflow).
/// Empty/whitespace-only text appends nothing. Each word-window is additionally capped to
/// [`MAX_CHUNK_CHARS`] chars (splitting oversized windows into multiple chunks) so a file
/// with very few whitespace-separated "words" can't produce a chunk that overflows the
/// embedder's context window.
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
        let window = words[start..end].join(" ");
        // A `size`-word window can still be enormous (minified CSS = one giant "word";
        // long unbroken lines). Split oversized windows on a char budget.
        for piece in split_char_budget(&window, MAX_CHUNK_CHARS) {
            chunks.push(Chunk {
                source: source.to_path_buf(),
                seq: *seq,
                heading: heading.to_owned(),
                text: piece.to_owned(),
                language: language.map(|s| s.to_owned()),
            });
            *seq += 1;
        }
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
    fn split_char_budget_splits_long_input() {
        let s = "x".repeat(10_000);
        let pieces = split_char_budget(&s, MAX_CHUNK_CHARS);
        assert!(
            pieces.len() >= 3,
            "expected ≥3 pieces, got {}",
            pieces.len()
        );
        assert!(pieces.iter().all(|p| p.chars().count() <= MAX_CHUNK_CHARS));
        assert_eq!(pieces.concat(), s); // lossless
    }

    #[test]
    fn split_char_budget_short_input_is_one_piece() {
        let pieces = split_char_budget("hello world", MAX_CHUNK_CHARS);
        assert_eq!(pieces, vec!["hello world"]);
    }

    #[test]
    fn chunk_words_caps_minified_single_token() {
        // A minified CSS/HTML line: one giant "word" (no whitespace) of 20k chars.
        // It must be split into multiple chunks, none exceeding MAX_CHUNK_CHARS,
        // instead of one oversized chunk that 500s the embedder.
        let text = "a".repeat(20_000);
        let mut seq = 0;
        let mut chunks = Vec::new();
        chunk_words(
            Path::new("/min.css"),
            &text,
            "",
            None,
            800,
            100,
            &mut seq,
            &mut chunks,
        );
        assert!(
            chunks.len() >= 5,
            "expected several chunks, got {}",
            chunks.len()
        );
        assert!(chunks
            .iter()
            .all(|c| c.text.chars().count() <= MAX_CHUNK_CHARS));
    }

    #[test]
    fn split_char_budget_respects_utf8_boundaries() {
        // Multi-byte chars must never be cut mid-codepoint.
        let s = "é".repeat(6000); // 2 bytes each, 6000 chars
        let pieces = split_char_budget(&s, MAX_CHUNK_CHARS);
        assert!(pieces.iter().all(|p| p.chars().count() <= MAX_CHUNK_CHARS));
        assert_eq!(pieces.concat(), s);
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
