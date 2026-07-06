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

/// A code-relationship-graph edge emitted by a parser (currently only the code parser):
/// `from` imports a module / defines a symbol named `to`. `kind` is `"imports"` or
/// `"defines"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: PathBuf,
    pub kind: &'static str,
    pub to: String,
}

/// Output of a parser run on one file.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub source: PathBuf,
    pub mime: String,
    pub chunks: Vec<Chunk>,
    /// Code-graph edges (empty for non-code parsers).
    pub edges: Vec<Edge>,
}

/// Hard ceiling on a single chunk's length in **chars**, independent of the word
/// window. `nomic-embed-text`'s context is ~2048 tokens; ~4000 chars ≈ ~1000 tokens,
/// leaving generous headroom. Without this cap a file with few whitespace-separated
/// "words" — minified CSS/HTML, or long single lines — emits one giant chunk that
/// makes Ollama 500 with "the input length exceeds the context length".
pub const MAX_CHUNK_CHARS: usize = 4000;

/// Per-zip-entry decompressed-size cap (16 MiB). A zip header's declared uncompressed size is
/// **untrusted** — a "zip bomb" declares little but decompresses to gigabytes — so container
/// parsers bound the *actual* decompressed read via [`read_zip_entry_text`] /
/// [`read_zip_entry_bytes`] rather than trusting `ZipFile::size()`. Real Office/EPUB parts are
/// typically well under 4 MiB, so 16 MiB is generous headroom while capping a single bomb entry.
pub const MAX_ZIP_ENTRY_BYTES: u64 = 16 * 1024 * 1024;

/// Running-total decompression cap (64 MiB) for multi-entry containers (EPUB spines, PPTX decks):
/// extraction stops once the cumulative decompressed size crosses this, so many individually-legal
/// entries can't sum to an OOM even when each fits under [`MAX_ZIP_ENTRY_BYTES`].
pub const MAX_ZIP_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Read a zip entry's decompressed bytes into a lossy UTF-8 string with a hard byte cap. Bounds
/// the *read* (never trusts the declared size), and uses lossy decoding so a cap that lands
/// mid-multibyte can't hard-fail an otherwise-valid document. Use for text parts (XML/XHTML).
pub fn read_zip_entry_text<R: std::io::Read>(entry: R, cap: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    entry.take(cap).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read a zip entry's raw decompressed bytes with a hard byte cap (see [`read_zip_entry_text`]).
/// Use for binary parts (e.g. an embedded preview PDF).
pub fn read_zip_entry_bytes<R: std::io::Read>(entry: R, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    entry.take(cap).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Chunk sizing knobs threaded from `[chunking]` config into the word-window parsers.
/// `size` is the target words per chunk; `overlap` is the words shared between consecutive
/// windows. [`Default`] is the historical `800`/`100` so every free-function / `Registry::new`
/// path stays behavior-neutral.
#[derive(Debug, Clone, Copy)]
pub struct ChunkParams {
    pub size: usize,
    pub overlap: usize,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self {
            size: 800,
            overlap: 100,
        }
    }
}

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

/// How fully a parser extracts a format — surfaced by `indexa formats` so the
/// "understands every file" claim stays queryable and honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Content is parsed into searchable text (code, PDF text layer, Office, HTML, …).
    Full,
    /// Only metadata/listing is extracted (image EXIF, media ffprobe, archive entry names).
    Metadata,
    /// Recognised but not extracted — a quiet placeholder (e.g. legacy OLE binaries).
    Stub,
    /// Sniffed as UTF-8 text with no dedicated parser (extensionless / unknown text files).
    TextFallback,
}

impl Support {
    pub fn as_str(&self) -> &'static str {
        match self {
            Support::Full => "full",
            Support::Metadata => "metadata",
            Support::Stub => "stub",
            Support::TextFallback => "textfallback",
        }
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

    /// Parse honoring caller-supplied chunk sizing (from `[chunking]` config).
    /// Default delegates to [`parse`](Parser::parse) and ignores `chunk`, so parsers that don't
    /// word-window (code/image/media) and external/plugin parsers need no change. Word-window
    /// parsers OVERRIDE this to thread `chunk.size`/`chunk.overlap` into their chunker, and make
    /// their `parse` delegate here with [`ChunkParams::default`].
    ///
    /// NOTE: if you override this **and** make `parse` call it, you MUST override this — otherwise
    /// the default `parse_chunked → parse → parse_chunked` recurses forever.
    fn parse_chunked(
        &self,
        path: &std::path::Path,
        chunk: ChunkParams,
    ) -> anyhow::Result<Extracted> {
        let _ = chunk;
        self.parse(path)
    }

    /// The `(extension, support level)` pairs this parser advertises, for `indexa formats`.
    /// Default: none (parsers matched by MIME only, or that don't want to be advertised).
    /// A parser with mixed levels (e.g. Office: `.docx` Full but legacy `.ppt` Stub) lists
    /// each extension with its true level.
    fn declared_formats(&self) -> &'static [(&'static str, Support)] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn read_zip_entry_text_bounds_the_read_not_the_declared_size() {
        // A "1000-byte entry" read with a 100-byte cap yields exactly 100 bytes — proving the
        // read is bounded by the cap, not by however large the source claims to be (zip-bomb guard).
        let big = vec![b'a'; 1000];
        let s = read_zip_entry_text(std::io::Cursor::new(big), 100).unwrap();
        assert_eq!(s.len(), 100);
    }

    #[test]
    fn read_zip_entry_text_returns_full_content_under_cap() {
        let small = b"hello world".to_vec();
        let s = read_zip_entry_text(std::io::Cursor::new(small), MAX_ZIP_ENTRY_BYTES).unwrap();
        assert_eq!(s, "hello world");
    }

    #[test]
    fn read_zip_entry_bytes_caps_binary_reads() {
        let big = vec![0u8; 5000];
        let b = read_zip_entry_bytes(std::io::Cursor::new(big), 256).unwrap();
        assert_eq!(b.len(), 256);
    }

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
