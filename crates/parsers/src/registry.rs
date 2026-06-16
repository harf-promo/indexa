//! Parser registry — routes paths to the right parser by extension/MIME.
//!
//! # Plugin SDK
//!
//! Third-party parsers can extend the registry at compile time by implementing
//! [`crate::types::Parser`] and registering via [`Registry::register`]:
//!
//! ```rust,ignore
//! use indexa_parsers::registry::Registry;
//! use indexa_parsers::types::{Chunk, Extracted, Parser};
//!
//! struct MyParser;
//! impl Parser for MyParser {
//!     fn accepts_mime(&self, mime: &str) -> bool { mime == "application/x-mything" }
//!     fn parse(&self, path: &std::path::Path) -> anyhow::Result<Extracted> {
//!         // ... read the file and return chunks ...
//!         Ok(Extracted { source: path.to_path_buf(), mime: "application/x-mything".into(),
//!             chunks: vec![], edges: vec![] })
//!     }
//! }
//!
//! // In your custom indexa binary:
//! let mut reg = Registry::new();
//! reg.register(Box::new(MyParser));
//! let extracted = reg.parse(path)?;
//! ```
//!
//! Custom parsers are inserted **before** the built-in fallbacks, so they take
//! precedence for any MIME type they claim.

use crate::archive::ArchiveParser;
use crate::code::CodeParser;
use crate::email::EmailParser;
use crate::epub::EpubParser;
use crate::html::HtmlParser;
use crate::image::ImageParser;
use crate::ipynb::IpynbParser;
use crate::media::MediaParser;
use crate::office::OfficeParser;
use crate::org::OrgParser;
use crate::pdf::PdfParser;
use crate::presentation::PresentationParser;
use crate::svg::SvgParser;
use crate::text::{MarkdownParser, TextParser};
use crate::types::{Extracted, Parser};
use anyhow::{bail, Result};
use std::path::Path;

// ── Registry struct ───────────────────────────────────────────────────────────

/// An extensible parser registry.
///
/// [`Registry::new`] populates all built-in parsers. Call [`Registry::register`]
/// to prepend additional parsers (e.g. third-party plugin parsers) before the
/// built-ins. Custom parsers are checked **first**; built-ins serve as fallbacks.
///
/// For one-shot parsing without customisation, use the free-function [`parse`].
pub struct Registry {
    parsers: Vec<Box<dyn Parser>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// Build a registry pre-loaded with all built-in Indexa parsers.
    pub fn new() -> Self {
        Self {
            parsers: vec![
                Box::new(IpynbParser), // .ipynb (JSON) — by extension, before generic text
                Box::new(CodeParser),
                Box::new(PdfParser),
                Box::new(EpubParser),
                Box::new(OrgParser::default()),
                Box::new(SvgParser), // must precede ImageParser (image/svg+xml)
                Box::new(ImageParser),
                Box::new(MediaParser),
                Box::new(PresentationParser), // must precede OfficeParser (pptx vs. ppt)
                Box::new(OfficeParser),
                Box::new(EmailParser),           // .eml/.msg
                Box::new(ArchiveParser), // .zip/.tar/.tar.gz — after office/epub so it never claims their zip containers
                Box::new(HtmlParser::default()), // .html/.htm → Markdown, before the text fallback
                Box::new(MarkdownParser::default()),
                Box::new(TextParser::default()),
            ],
        }
    }

    /// Register a custom parser. It is inserted **before** the built-ins so it
    /// takes priority for any MIME type / extension it claims.
    pub fn register(&mut self, parser: Box<dyn Parser>) {
        self.parsers.insert(0, parser);
    }

    /// Parse `path` using the first matching parser in the registry.
    pub fn parse(&self, path: &Path) -> Result<Extracted> {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        dispatch(&self.parsers, path, &mime)
    }

    /// Parse `path` with size and panic guards (see [`parse_guarded`]), using this
    /// registry's parsers (including any registered via [`Registry::register`]).
    pub fn parse_guarded(&self, path: &Path, size_bytes: u64, max_bytes: u64) -> Result<Extracted> {
        if max_bytes > 0 && size_bytes > max_bytes {
            bail!(
                "skipping {} for parsing: {size_bytes} bytes exceeds the {max_bytes}-byte cap",
                path.display()
            );
        }
        // Dispatch against THIS registry (not the free `parse`) so custom parsers are
        // honoured on the guarded path too. catch_unwind isolates a panicking parser.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.parse(path))) {
            Ok(result) => result,
            Err(_) => bail!("parser panicked on {}", path.display()),
        }
    }
}

// ── Free-function API (backward-compatible) ────────────────────────────────────

/// Convenience: parse `path` using the default built-in registry.
/// Most callers should use this; use [`Registry`] only for plugin extension.
///
/// Delegates to [`Registry::new`] so the built-in parser list lives in exactly one place —
/// a new parser is added there and is picked up by both this free function and `Registry`.
pub fn parse(path: &Path) -> Result<Extracted> {
    Registry::new().parse(path)
}

/// Core dispatch logic shared by [`Registry::parse`] and the free-function [`parse`].
fn dispatch(parsers: &[Box<dyn Parser>], path: &Path, mime: &str) -> Result<Extracted> {
    // Prefer path-aware acceptance (handles extensions mime_guess gets wrong).
    if let Some(p) = parsers.iter().find(|p| p.accepts_path(path)) {
        return p.parse(path);
    }

    // MIME-based fallback.
    if let Some(p) = parsers.iter().find(|p| p.accepts_mime(mime)) {
        return p.parse(path);
    }

    // Last resort: plain text for text/* MIME types.
    if mime.starts_with("text/") {
        return TextParser::default().parse(path);
    }

    // Many text files have no extension, or a name `mime_guess` maps to
    // octet-stream (LICENSE, NOTICE, Cargo.lock, .gitignore, …). Sniff the first
    // bytes: if it looks like UTF-8 text with no NUL byte, index it as plain text
    // instead of warning "no parser".
    if looks_like_text(path) {
        return TextParser::default().parse(path);
    }

    bail!("no parser for: {} (MIME: {mime})", path.display());
}

/// Cheap heuristic: read the first ~8 KB and decide whether the file is text.
/// True when there is no NUL byte and the bytes are valid UTF-8 (allowing only a
/// final multi-byte char to be cut off by the 8 KB read). Genuinely binary files
/// (NUL bytes, non-UTF-8) return false so they still `bail!` upstream.
fn looks_like_text(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8192];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    if n == 0 {
        return true; // empty file: harmless to treat as (empty) text
    }
    let slice = &buf[..n];
    if slice.contains(&0) {
        return false; // NUL byte → binary
    }
    match std::str::from_utf8(slice) {
        Ok(_) => true,
        // Tolerate only a trailing partial char clipped by the 8 KB window.
        Err(e) => e.valid_up_to() >= n.saturating_sub(4),
    }
}

/// Parse a file with two safety guards, returning `Err` (never panicking, never
/// reading an oversized file) so one bad file can't abort a whole scan:
///
/// 1. **Size cap** — files larger than `max_bytes` are skipped (`max_bytes == 0`
///    disables the cap). Every content parser reads the whole file into memory, so
///    an accidental multi-GB log/CSV/binary misclassified as text would otherwise
///    exhaust RAM mid-scan.
/// 2. **Panic isolation** — third-party parser internals (e.g. `pdf-extract`/`lopdf`
///    on a malformed PDF) can panic on adversarial input. `catch_unwind` converts a
///    panic into an `Err` so the caller can log it and move to the next file.
pub fn parse_guarded(path: &Path, size_bytes: u64, max_bytes: u64) -> Result<Extracted> {
    if max_bytes > 0 && size_bytes > max_bytes {
        bail!(
            "skipping {} for parsing: {size_bytes} bytes exceeds the {max_bytes}-byte cap",
            path.display()
        );
    }
    let path2 = path.to_path_buf();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse(&path2))) {
        Ok(result) => result,
        Err(_) => bail!("parser panicked on {}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_text_accepts_extensionless_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("LICENSE");
        std::fs::write(&p, "MIT License\n\nPermission is hereby granted...").unwrap();
        assert!(looks_like_text(&p));
    }

    #[test]
    fn looks_like_text_rejects_binary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob.bin");
        std::fs::write(&p, [0u8, 1, 2, 0, 255, 254]).unwrap();
        assert!(!looks_like_text(&p));
    }

    #[test]
    fn parse_indexes_extensionless_text_via_sniff() {
        // LICENSE/NOTICE/Cargo.lock map to octet-stream in mime_guess; the sniff
        // fallback must parse them as text rather than bail "no parser".
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("NOTICE");
        std::fs::write(&p, "This product includes software developed by Indexa.").unwrap();
        let ex = parse(&p).expect("extension-less text file should parse via sniff");
        assert!(!ex.chunks.is_empty());
    }

    #[test]
    fn parse_guarded_skips_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        std::fs::write(&p, "small but real content").unwrap();
        let size = std::fs::metadata(&p).unwrap().len();

        // A cap below the file size → skipped (Err), file never read.
        assert!(parse_guarded(&p, size, 1).is_err());
        // 0 disables the cap → parses fine.
        assert!(parse_guarded(&p, size, 0).is_ok());
        // A generous cap → parses fine.
        assert!(parse_guarded(&p, size, 10_000_000).is_ok());
    }

    // ── Plugin SDK: Registry ──────────────────────────────────────────────────

    /// A custom plugin parser claiming the `.mydata` extension.
    struct MyDataParser;
    impl Parser for MyDataParser {
        fn accepts_path(&self, path: &Path) -> bool {
            path.extension().and_then(|e| e.to_str()) == Some("mydata")
        }
        fn accepts_mime(&self, _mime: &str) -> bool {
            false
        }
        fn parse(&self, path: &Path) -> Result<Extracted> {
            Ok(Extracted {
                source: path.to_path_buf(),
                mime: "application/x-mydata".to_owned(),
                chunks: vec![crate::types::Chunk {
                    source: path.to_path_buf(),
                    seq: 0,
                    heading: String::new(),
                    text: "CUSTOM-PARSED".to_owned(),
                    language: None,
                }],
                edges: Vec::new(),
            })
        }
    }

    #[test]
    fn registry_register_routes_to_custom_parser() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sample.mydata");
        std::fs::write(&p, "raw bytes the custom parser ignores").unwrap();

        // Default registry has no parser claiming .mydata → falls back to text sniff.
        let default_reg = Registry::new();
        let ex_default = default_reg.parse(&p).unwrap();
        assert_ne!(ex_default.chunks[0].text, "CUSTOM-PARSED");

        // After registering the plugin, .mydata routes to it.
        let mut reg = Registry::new();
        reg.register(Box::new(MyDataParser));
        let ex = reg.parse(&p).unwrap();
        assert_eq!(ex.chunks[0].text, "CUSTOM-PARSED");
        assert_eq!(ex.mime, "application/x-mydata");
    }

    #[test]
    fn registry_custom_parser_takes_precedence_over_builtins() {
        // A custom parser claiming .txt must win over the built-in TextParser,
        // because register() inserts before the built-ins.
        struct TxtClaimer;
        impl Parser for TxtClaimer {
            fn accepts_path(&self, path: &Path) -> bool {
                path.extension().and_then(|e| e.to_str()) == Some("txt")
            }
            fn accepts_mime(&self, _mime: &str) -> bool {
                false
            }
            fn parse(&self, path: &Path) -> Result<Extracted> {
                Ok(Extracted {
                    source: path.to_path_buf(),
                    mime: "text/x-claimed".to_owned(),
                    chunks: vec![crate::types::Chunk {
                        source: path.to_path_buf(),
                        seq: 0,
                        heading: String::new(),
                        text: "CLAIMED".to_owned(),
                        language: None,
                    }],
                    edges: Vec::new(),
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        std::fs::write(&p, "hello").unwrap();

        let mut reg = Registry::new();
        reg.register(Box::new(TxtClaimer));
        let ex = reg.parse(&p).unwrap();
        assert_eq!(
            ex.chunks[0].text, "CLAIMED",
            "custom parser must beat built-in TextParser"
        );
    }

    #[test]
    fn registry_parse_guarded_honours_custom_parser() {
        // The guarded path must also route to registered parsers (not the free `parse`).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sample.mydata");
        std::fs::write(&p, "ignored").unwrap();
        let size = std::fs::metadata(&p).unwrap().len();

        let mut reg = Registry::new();
        reg.register(Box::new(MyDataParser));
        let ex = reg.parse_guarded(&p, size, 0).unwrap();
        assert_eq!(ex.chunks[0].text, "CUSTOM-PARSED");
    }
}
