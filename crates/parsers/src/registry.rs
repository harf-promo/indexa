//! Parser registry — routes paths to the right parser by MIME type.

use crate::text::{MarkdownParser, TextParser};
use crate::types::{Extracted, Parser};
use anyhow::{bail, Result};
use std::path::Path;

/// Returns an `Extracted` result for any supported file.
/// Falls back to `TextParser` for unknown text-like files.
pub fn parse(path: &Path) -> Result<Extracted> {
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    let parsers: Vec<Box<dyn Parser>> = vec![
        Box::new(MarkdownParser::default()),
        Box::new(TextParser::default()),
    ];

    if let Some(p) = parsers.iter().find(|p| p.accepts(&mime)) {
        return p.parse(path);
    }

    // Fallback: try reading as UTF-8 text.
    if mime.starts_with("text/") {
        return TextParser::default().parse(path);
    }

    bail!("no parser for MIME type: {mime}");
}
