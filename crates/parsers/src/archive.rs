//! Archive parser: list the entries of a `.zip` / `.tar` / `.tar.gz` so an archive is
//! searchable by the files it contains.
//!
//! **Shallow by default** — it emits entry names + sizes, not their content. Reading every
//! entry's bytes (and recursing into nested archives) is a future opt-in; listing avoids
//! zip-bomb blow-ups and keeps indexing cheap. Encrypted archives are not opened.
//!
//! Registered *after* the Office/EPUB parsers, and matched on the full `.zip`/`.tar`/`.tar.gz`
//! /`.tgz` name, so it never hijacks the zip-container formats those own (docx/xlsx/pptx/epub/odt).

use crate::types::{chunk_words, Chunk, ChunkParams, Extracted, Parser};
use anyhow::Result;
use std::cell::Cell;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

pub struct ArchiveParser;

/// Cap the listing so a pathological archive with millions of entries can't blow up memory.
const MAX_ENTRIES: usize = 5000;

/// Cap the bytes a `.tar.gz` listing may inflate. A tar stream has no index, so reaching the
/// next header means decompressing every byte of the entry before it: one small file holding a
/// huge zero-filled entry would otherwise pin a CPU for minutes, and the size guard upstream
/// only sees the compressed size.
const MAX_TAR_GZ_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

/// Why a listing stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truncation {
    Entries,
    Bytes,
}

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
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let name = file_name_lower(path);
        let (entries, mime) = if name.ends_with(".zip") {
            (list_zip(path), "application/zip")
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            (list_tar(path, true), "application/gzip")
        } else {
            (list_tar(path, false), "application/x-tar")
        };
        let (entries, truncated) = entries.unwrap_or_default();

        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let listing = if entries.is_empty() {
            format!("Archive: {display} (empty, encrypted, or unreadable)")
        } else {
            let mut s = format!(
                "Archive {display} — {} entries:\n{}",
                entries.len(),
                entries.join("\n")
            );
            // The cap skips directory rows, so entries.len() can be below MAX_ENTRIES even
            // when truncated — rely on the explicit flag, not the row count, to stay honest.
            match truncated {
                Some(Truncation::Entries) => s.push_str(&format!(
                    "\n(listing truncated — showing first {MAX_ENTRIES})"
                )),
                Some(Truncation::Bytes) => s.push_str(&format!(
                    "\n(listing truncated — stopped after {} MiB decompressed)",
                    MAX_TAR_GZ_DECOMPRESSED_BYTES / (1024 * 1024)
                )),
                None => {}
            }
            s
        };

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        chunk_words(
            path,
            &listing,
            "contents",
            None,
            chunk.size,
            chunk.overlap,
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

/// Returns `(entry listing, truncation)`. Truncated when the archive held more than
/// `MAX_ENTRIES` entries and the listing was capped.
fn list_zip(path: &Path) -> Result<(Vec<String>, Option<Truncation>)> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let truncated = (zip.len() > MAX_ENTRIES).then_some(Truncation::Entries);
    let n = zip.len().min(MAX_ENTRIES);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Ok(f) = zip.by_index(i) {
            if !f.is_dir() {
                out.push(format!("{} ({} bytes)", f.name(), f.size()));
            }
        }
    }
    Ok((out, truncated))
}

/// Returns `(entry listing, truncation)`. A tar entry stream has no length, so entry
/// truncation is detected by the iterator still yielding once we've consumed `MAX_ENTRIES`
/// entries. A `.tar.gz` also stops, with the entries listed so far, once it has inflated
/// `MAX_TAR_GZ_DECOMPRESSED_BYTES`.
fn list_tar(path: &Path, gz: bool) -> Result<(Vec<String>, Option<Truncation>)> {
    list_tar_with_budget(path, gz, MAX_TAR_GZ_DECOMPRESSED_BYTES)
}

fn list_tar_with_budget(
    path: &Path,
    gz: bool,
    gz_budget: u64,
) -> Result<(Vec<String>, Option<Truncation>)> {
    let file = std::fs::File::open(path)?;
    let exhausted = Rc::new(Cell::new(false));
    let reader: Box<dyn Read> = if gz {
        Box::new(BudgetReader {
            inner: flate2::read::GzDecoder::new(file),
            remaining: gz_budget,
            exhausted: Rc::clone(&exhausted),
        })
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    let mut out = Vec::new();
    let mut truncated = None;
    for (i, entry) in archive.entries()?.enumerate() {
        if exhausted.get() {
            truncated = Some(Truncation::Bytes);
            break;
        }
        if i >= MAX_ENTRIES {
            truncated = Some(Truncation::Entries);
            break;
        }
        let Ok(entry) = entry else { continue };
        let size = entry.header().size().unwrap_or(0);
        let Ok(p) = entry.path() else { continue };
        let ps = p.to_string_lossy();
        if !ps.ends_with('/') {
            out.push(format!("{ps} ({size} bytes)"));
        }
    }
    // The budget can run out inside the stream's final read, after which the iterator just
    // ends (tar fuses on the first error) without yielding again.
    if exhausted.get() {
        truncated = Some(Truncation::Bytes);
    }
    Ok((out, truncated))
}

/// A `Read` that fails once `remaining` bytes have passed through it, flagging `exhausted` so
/// the caller can tell a spent budget from a corrupt stream.
struct BudgetReader<R> {
    inner: R,
    remaining: u64,
    exhausted: Rc<Cell<bool>>,
}

impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            self.exhausted.set(true);
            return Err(std::io::Error::other("decompressed-size budget exhausted"));
        }
        let max = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
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
        // A small archive must NOT claim truncation.
        assert!(!all.contains("truncated"), "{all}");
    }

    #[test]
    fn zip_over_cap_reports_truncation() {
        use zip::write::FileOptions;
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        // One more than the cap so the listing is capped and the notice fires.
        for i in 0..(MAX_ENTRIES + 5) {
            zip.start_file(format!("f{i}.txt"), opts).unwrap();
            zip.write_all(b"x").unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("huge.zip");
        std::fs::write(&p, bytes).unwrap();

        let ex = ArchiveParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            all.contains("listing truncated"),
            "expected truncation notice"
        );
        assert!(
            all.contains(&MAX_ENTRIES.to_string()),
            "should name the cap"
        );
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

    /// A `.tar.gz` whose first entry is `zeros` bytes of zeros (a few KB compressed), followed by
    /// a small `after.txt` that can only be reached by inflating the whole first entry.
    fn zero_bomb_tgz(zeros: u64) -> tempfile::TempDir {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(zeros);
        header.set_mode(0o644);
        header.set_cksum();
        b.append_data(&mut header, "bomb.bin", std::io::repeat(0).take(zeros))
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        b.append_data(&mut header, "after.txt", &b"hi"[..]).unwrap();
        let gz = b.into_inner().unwrap().finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bomb.tar.gz"), gz).unwrap();
        dir
    }

    #[test]
    fn tar_gz_zero_bomb_stops_at_the_decompressed_budget() {
        let dir = zero_bomb_tgz(8 * 1024 * 1024);
        let p = dir.path().join("bomb.tar.gz");
        let (entries, truncated) = list_tar_with_budget(&p, true, 1024 * 1024).unwrap();
        assert_eq!(truncated, Some(Truncation::Bytes), "{entries:?}");
        assert!(
            entries.iter().any(|e| e.starts_with("bomb.bin")),
            "{entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.starts_with("after.txt")),
            "listing must stop at the budget, not inflate past it: {entries:?}"
        );
    }

    #[test]
    fn tar_gz_within_the_budget_lists_everything() {
        let dir = zero_bomb_tgz(8 * 1024 * 1024);
        let p = dir.path().join("bomb.tar.gz");
        let (entries, truncated) = list_tar_with_budget(&p, true, 64 * 1024 * 1024).unwrap();
        assert_eq!(truncated, None, "{entries:?}");
        assert!(
            entries.iter().any(|e| e.starts_with("after.txt")),
            "{entries:?}"
        );
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
