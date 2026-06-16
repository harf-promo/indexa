//! Archive parser: list the entries of a `.zip` / `.tar` / `.tar.gz` so an archive is
//! searchable by the files it contains.
//!
//! **Shallow by default** — it emits entry names + sizes, not their content. Reading every
//! entry's bytes (and recursing into nested archives) is a future opt-in; listing avoids
//! zip-bomb blow-ups and keeps indexing cheap. Encrypted archives are not opened.
//!
//! Registered *after* the Office/EPUB parsers, and matched on the full `.zip`/`.tar`/`.tar.gz`
//! /`.tgz` name, so it never hijacks the zip-container formats those own (docx/xlsx/pptx/epub/odt).

use crate::types::{chunk_words, Chunk, Extracted, Parser};
use anyhow::Result;
use std::io::Read;
use std::path::Path;

pub struct ArchiveParser;

/// Cap the listing so a pathological archive with millions of entries can't blow up memory.
const MAX_ENTRIES: usize = 5000;

impl Parser for ArchiveParser {
    fn accepts_path(&self, path: &Path) -> bool {
        let name = file_name_lower(path);
        name.ends_with(".zip")
            || name.ends_with(".tar")
            || name.ends_with(".tar.gz")
            || name.ends_with(".tgz")
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        matches!(
            mime,
            "application/zip" | "application/x-tar" | "application/gzip" | "application/x-gtar"
        )
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[
            ("zip", Metadata),
            ("tar", Metadata),
            ("tar.gz", Metadata),
            ("tgz", Metadata),
        ]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let name = file_name_lower(path);
        let (entries, mime) = if name.ends_with(".zip") {
            (list_zip(path), "application/zip")
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            (list_tar(path, true), "application/gzip")
        } else {
            (list_tar(path, false), "application/x-tar")
        };
        let entries = entries.unwrap_or_default();

        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let listing = if entries.is_empty() {
            format!("Archive: {display} (empty, encrypted, or unreadable)")
        } else {
            format!(
                "Archive {display} — {} entries:\n{}",
                entries.len(),
                entries.join("\n")
            )
        };

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        chunk_words(
            path,
            &listing,
            "contents",
            None,
            800,
            100,
            &mut seq,
            &mut chunks,
        );
        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!("Archive: {display}"),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: mime.into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn list_zip(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let n = zip.len().min(MAX_ENTRIES);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Ok(f) = zip.by_index(i) {
            if !f.is_dir() {
                out.push(format!("{} ({} bytes)", f.name(), f.size()));
            }
        }
    }
    Ok(out)
}

fn list_tar(path: &Path, gz: bool) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    let reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    let mut out = Vec::new();
    for entry in archive.entries()?.take(MAX_ENTRIES) {
        let Ok(entry) = entry else { continue };
        let size = entry.header().size().unwrap_or(0);
        let Ok(p) = entry.path() else { continue };
        let ps = p.to_string_lossy();
        if !ps.ends_with('/') {
            out.push(format!("{ps} ({size} bytes)"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn zip_lists_entries() {
        use zip::write::FileOptions;
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));
        let opts = FileOptions::<()>::default();
        zip.start_file("src/main.rs", opts).unwrap();
        zip.write_all(b"fn main() {}").unwrap();
        zip.start_file("README.md", opts).unwrap();
        zip.write_all(b"# Hi").unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bundle.zip");
        std::fs::write(&p, bytes).unwrap();

        let ex = ArchiveParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("src/main.rs"), "{all}");
        assert!(all.contains("README.md"), "{all}");
        assert!(all.contains("entries"), "{all}");
    }

    #[test]
    fn tar_gz_lists_entries() {
        // Build a .tar.gz in memory.
        let tar_buf = {
            let mut b = tar::Builder::new(Vec::new());
            let data = b"hello";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            b.append_data(&mut header, "notes/a.txt", &data[..])
                .unwrap();
            b.into_inner().unwrap()
        };
        let gz = {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("archive.tar.gz");
        std::fs::write(&p, gz).unwrap();

        let ex = ArchiveParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("notes/a.txt"), "{all}");
    }

    #[test]
    fn accepts_archive_names_not_office_zips() {
        let p = ArchiveParser;
        assert!(p.accepts_path(Path::new("/x/data.zip")));
        assert!(p.accepts_path(Path::new("/x/release.tar.gz")));
        assert!(p.accepts_path(Path::new("/x/release.tgz")));
        assert!(p.accepts_path(Path::new("/x/backup.tar")));
        // Office/EPUB zip-containers are NOT claimed by the archive parser.
        assert!(!p.accepts_path(Path::new("/x/report.docx")));
        assert!(!p.accepts_path(Path::new("/x/book.epub")));
        assert!(!p.accepts_path(Path::new("/x/sheet.xlsx")));
    }
}
