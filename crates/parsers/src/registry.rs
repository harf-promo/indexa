//! Parser registry — routes paths to the right parser by extension/MIME.

use crate::code::CodeParser;
use crate::epub::EpubParser;
use crate::image::ImageParser;
use crate::media::MediaParser;
use crate::office::OfficeParser;
use crate::org::OrgParser;
use crate::pdf::PdfParser;
use crate::text::{MarkdownParser, TextParser};
use crate::types::{Extracted, Parser};
use anyhow::{bail, Result};
use std::path::Path;

/// Returns an `Extracted` result for any supported file.
/// Priority: extension-based parsers (exact-match) → MIME-based fallback → plain-text.
pub fn parse(path: &Path) -> Result<Extracted> {
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    let parsers: Vec<Box<dyn Parser>> = vec![
        Box::new(CodeParser),
        Box::new(PdfParser),
        Box::new(EpubParser),
        Box::new(OrgParser::default()),
        Box::new(ImageParser),
        Box::new(MediaParser),
        Box::new(OfficeParser),
        Box::new(MarkdownParser::default()),
        Box::new(TextParser::default()),
    ];

    // Prefer path-aware acceptance (handles extensions mime_guess gets wrong).
    if let Some(p) = parsers.iter().find(|p| p.accepts_path(path)) {
        return p.parse(path);
    }

    // MIME-based fallback.
    if let Some(p) = parsers.iter().find(|p| p.accepts_mime(&mime)) {
        return p.parse(path);
    }

    // Last resort: plain text for text/* MIME types.
    if mime.starts_with("text/") {
        return TextParser::default().parse(path);
    }

    bail!("no parser for: {} (MIME: {mime})", path.display());
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse(path))) {
        Ok(result) => result,
        Err(_) => bail!("parser panicked on {}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
