//! Apple iWork (`.pages` / `.numbers` / `.key`): modern iWork files are zip packages that embed
//! a rendered `preview.pdf` (or `QuickLook/Preview.pdf`). We extract that PDF and run it through
//! the existing PDF text path — **zero new dependency**. The native IWA (protobuf) payload has no
//! maintained pure-Rust reader, so this captures the rendered snapshot's text — not every cell or
//! formula — and a file without a preview yields a quiet stub.

use crate::types::{chunk_words, Chunk, Extracted, Parser};
use anyhow::Result;
use std::io::Read;
use std::path::Path;

pub struct IworkParser;

impl Parser for IworkParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pages" | "numbers" | "key")
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        matches!(
            mime,
            "application/vnd.apple.pages"
                | "application/vnd.apple.numbers"
                | "application/vnd.apple.keynote"
        )
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[("pages", Full), ("numbers", Full), ("key", Full)]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let mime = match path.extension().and_then(|e| e.to_str()) {
            Some("pages") => "application/vnd.apple.pages",
            Some("numbers") => "application/vnd.apple.numbers",
            Some("key") => "application/vnd.apple.keynote",
            _ => "application/octet-stream",
        };

        let text = extract_preview_text(path).unwrap_or_default();
        let mut chunks = Vec::new();
        let mut seq = 0usize;
        if text.trim().is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!("iWork document: {display} (no extractable preview text)"),
                language: None,
            });
        } else {
            chunk_words(path, &text, "", None, 800, 100, &mut seq, &mut chunks);
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: mime.into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Pull text from the embedded preview PDF (`preview.pdf` / `QuickLook/Preview.pdf`).
fn extract_preview_text(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    for name in ["preview.pdf", "QuickLook/Preview.pdf", "Preview.pdf"] {
        if let Ok(mut entry) = zip.by_name(name) {
            let mut buf = Vec::new();
            if entry.read_to_end(&mut buf).is_ok() {
                if let Ok(text) = pdf_extract::extract_text_from_mem(&buf) {
                    if !text.trim().is_empty() {
                        return Ok(text);
                    }
                }
            }
        }
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn iwork_accepts_extensions() {
        let p = IworkParser;
        assert!(p.accepts_path(Path::new("/x/report.pages")));
        assert!(p.accepts_path(Path::new("/x/budget.numbers")));
        assert!(p.accepts_path(Path::new("/x/deck.key")));
        assert!(!p.accepts_path(Path::new("/x/report.docx")));
    }

    #[test]
    fn iwork_without_preview_yields_stub() {
        // A zip package with no preview.pdf → quiet stub, never a hard error.
        use zip::write::FileOptions;
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));
        zip.start_file("Index/Document.iwa", FileOptions::<()>::default())
            .unwrap();
        zip.write_all(b"\x00\x01protobuf-ish").unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("doc.pages");
        std::fs::write(&p, bytes).unwrap();
        let ex = IworkParser.parse(&p).unwrap();
        assert_eq!(ex.chunks.len(), 1);
        assert!(ex.chunks[0].text.contains("iWork document"));
    }
}
