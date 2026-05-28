//! Parser registry — routes paths to the right parser by extension/MIME.

use crate::code::CodeParser;
use crate::image::ImageParser;
use crate::media::MediaParser;
use crate::office::OfficeParser;
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
