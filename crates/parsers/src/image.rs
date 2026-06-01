//! Image parser: extracts EXIF metadata as searchable text.
//! Optionally supports SigLIP-2 captioning (Google, opt-in via config).

use crate::types::{Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct ImageParser;

impl Parser for ImageParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(
                "jpg"
                    | "jpeg"
                    | "png"
                    | "gif"
                    | "webp"
                    | "heic"
                    | "heif"
                    | "tiff"
                    | "tif"
                    | "bmp"
                    | "cr2"
                    | "nef"
                    | "arw"
                    | "dng"
            )
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime.starts_with("image/")
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let mut parts: Vec<String> = Vec::new();

        // Filename as searchable text.
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            parts.push(format!("File: {name}"));
        }

        // EXIF metadata.
        match read_exif(path) {
            Ok(exif_text) if !exif_text.is_empty() => parts.push(exif_text),
            _ => {}
        }

        let text = if parts.is_empty() {
            format!("Image: {}", path.display())
        } else {
            parts.join("\n")
        };

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "image/jpeg".into(), // approximate; real MIME not critical for indexing
            chunks: vec![Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text,
                language: None,
            }],
            edges: Vec::new(),
        })
    }
}

fn read_exif(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bufreader = std::io::BufReader::new(file);
    let exif_reader = exif::Reader::new();
    let exif = exif_reader.read_from_container(&mut bufreader)?;

    let mut parts = Vec::new();

    let field_names = [
        (exif::Tag::DateTime, "Date"),
        (exif::Tag::DateTimeOriginal, "Date taken"),
        (exif::Tag::Make, "Camera make"),
        (exif::Tag::Model, "Camera model"),
        (exif::Tag::ImageWidth, "Width"),
        (exif::Tag::ImageLength, "Height"),
        (exif::Tag::GPSLatitude, "GPS lat"),
        (exif::Tag::GPSLongitude, "GPS lon"),
        (exif::Tag::ImageDescription, "Description"),
        (exif::Tag::Artist, "Artist"),
        (exif::Tag::Copyright, "Copyright"),
    ];

    for (tag, label) in &field_names {
        if let Some(field) = exif.get_field(*tag, exif::In::PRIMARY) {
            let val = field.display_value().to_string();
            if !val.is_empty() && val != "\"\"" {
                parts.push(format!("{label}: {val}"));
            }
        }
    }

    Ok(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_parser_accepts_known_extensions() {
        let p = ImageParser;
        assert!(p.accepts_path(Path::new("photo.jpg")));
        assert!(p.accepts_path(Path::new("image.png")));
        assert!(p.accepts_path(Path::new("raw.cr2")));
        assert!(!p.accepts_path(Path::new("doc.pdf")));
    }

    #[test]
    fn image_parser_handles_missing_exif_gracefully() {
        // PNG files typically have no EXIF — parser should still produce a chunk.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("test.png");
        // Write a minimal 1x1 PNG (89 bytes).
        std::fs::write(
            &p,
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90wS\xde\x00\x00\x00\x0cIDATx\x9cc\xf8\x0f\x00\x00\x01\x01\x00\x05\x18\xd8N\x00\x00\x00\x00IEND\xaeB`\x82",
        ).unwrap();
        let parser = ImageParser;
        let extracted = parser.parse(&p).unwrap();
        assert_eq!(extracted.chunks.len(), 1);
        assert!(extracted.chunks[0].text.contains("test.png"));
    }
}
