//! Transparent gzip content indexing (4.5, ripgrep `-z`) — `README.md.gz`, rotated
//! `.log.gz`, man pages get their DECOMPRESSED content indexed instead of being treated as
//! an opaque binary blob. gzip only (`flate2`, already in the tree) — zstd/xz/brotli are
//! new dependencies, deferred until demanded. Default OFF (`[parsers] compressed = false`,
//! enabled via `Registry::enable_compressed`). `.tar.gz`/`.tgz` stay with `archive.rs` —
//! this parser explicitly excludes them so it never hijacks that format.

use crate::registry::Registry;
use crate::types::{ChunkParams, Extracted, Parser, MAX_ZIP_ENTRY_BYTES};
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use std::io::Read;
use std::path::Path;

pub struct CompressedParser;

impl CompressedParser {
    /// The filename a `.gz` file's DECOMPRESSED content would have — `foo.log.gz` ->
    /// `foo.log` — so it can be routed through the normal registry dispatch (extension/MIME
    /// based), including 1.3's encoding handling for text. `None` for anything that isn't a
    /// standalone `.gz` (no `.gz` suffix, or a `.tar.gz`/`.tgz` archive — `archive.rs`'s job).
    fn inner_name(path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        if name.ends_with(".tar.gz") || name.ends_with(".tgz") || !name.ends_with(".gz") {
            return None;
        }
        Some(name[..name.len() - ".gz".len()].to_owned())
    }
}

impl Parser for CompressedParser {
    fn accepts_path(&self, path: &Path) -> bool {
        Self::inner_name(path).is_some()
    }

    // Matched by name only — a `.gz` file's own MIME (application/gzip) tells us nothing
    // about what's inside it.
    fn accepts_mime(&self, _mime: &str) -> bool {
        false
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let inner_name = Self::inner_name(path)
            .with_context(|| format!("{} is not a recognized .gz file", path.display()))?;

        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let decoder = GzDecoder::new(file);
        let mut buf = Vec::new();
        // Bomb-guarded: read one byte past the cap so an oversized decompression is
        // detected without buffering the whole (potentially huge) stream.
        decoder
            .take(MAX_ZIP_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .with_context(|| format!("decompressing {}", path.display()))?;
        if buf.len() as u64 > MAX_ZIP_ENTRY_BYTES {
            bail!(
                "{} decompresses to more than the {MAX_ZIP_ENTRY_BYTES}-byte cap — skipping \
                 content parse (metadata-only)",
                path.display()
            );
        }

        // Write the decompressed bytes to a scratch temp file named with the INNER
        // extension, so the normal registry dispatch routes it to the right native parser
        // purely by extension/MIME — parsers in this codebase read from a real path, they
        // don't accept pre-loaded bytes, so this is the natural integration point rather
        // than a new in-memory Parser API.
        let tmp_dir =
            tempfile::tempdir().context("creating a scratch dir for gzip decompression")?;
        let tmp_path = tmp_dir.path().join(&inner_name);
        std::fs::write(&tmp_path, &buf)
            .with_context(|| format!("writing decompressed content to {}", tmp_path.display()))?;

        // A fresh built-ins-only registry (no `CompressedParser` itself — double-gzip isn't
        // recursively unwrapped, matching the "singly-compressed" scope) with the same
        // chunk sizing as the caller's registry.
        let mut extracted = Registry::with_chunk(chunk)
            .parse(&tmp_path)
            .with_context(|| format!("parsing decompressed content of {}", path.display()))?;

        // Report the ORIGINAL .gz path as the source everywhere — the scratch temp path is
        // about to be deleted, and every downstream consumer (store writes, code-graph
        // edges) keys on `source`/`from`.
        extracted.source = path.to_path_buf();
        for c in &mut extracted.chunks {
            c.source = path.to_path_buf();
        }
        for e in &mut extracted.edges {
            e.from = path.to_path_buf();
        }
        Ok(extracted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn gzip_bytes(content: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn accepts_path_matches_gz_but_not_tar_gz_or_tgz() {
        let p = CompressedParser;
        assert!(p.accepts_path(Path::new("README.md.gz")));
        assert!(p.accepts_path(Path::new("access.log.gz")));
        assert!(!p.accepts_path(Path::new("archive.tar.gz")));
        assert!(!p.accepts_path(Path::new("archive.tgz")));
        assert!(!p.accepts_path(Path::new("plain.txt")));
        assert!(!p.accepts_mime("application/gzip"));
    }

    #[test]
    fn inner_name_strips_only_the_trailing_gz() {
        assert_eq!(
            CompressedParser::inner_name(Path::new("access.log.gz")),
            Some("access.log".to_owned())
        );
        assert_eq!(
            CompressedParser::inner_name(Path::new("README.md.gz")),
            Some("README.md".to_owned())
        );
        assert_eq!(
            CompressedParser::inner_name(Path::new("archive.tar.gz")),
            None
        );
        assert_eq!(CompressedParser::inner_name(Path::new("archive.tgz")), None);
    }

    #[test]
    fn parse_decompresses_and_routes_through_the_inner_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let gz_path = tmp.path().join("notes.md.gz");
        std::fs::write(&gz_path, gzip_bytes(b"# Heading\n\nSome body text.")).unwrap();

        let extracted = CompressedParser.parse(&gz_path).unwrap();
        assert!(!extracted.chunks.is_empty());
        assert!(extracted
            .chunks
            .iter()
            .any(|c| c.text.contains("Some body text.")));
        // Source is reported as the ORIGINAL .gz path, not the scratch temp file.
        assert_eq!(extracted.source, gz_path);
        for c in &extracted.chunks {
            assert_eq!(c.source, gz_path);
        }
    }

    #[test]
    fn parse_rejects_a_decompression_bomb_over_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let gz_path = tmp.path().join("bomb.txt.gz");
        // Highly compressible content that decompresses well past the cap.
        let huge = vec![b'a'; (MAX_ZIP_ENTRY_BYTES + 1024) as usize];
        std::fs::write(&gz_path, gzip_bytes(&huge)).unwrap();

        let result = CompressedParser.parse(&gz_path);
        assert!(
            result.is_err(),
            "must refuse to fully decompress an oversized payload"
        );
    }

    #[test]
    fn parse_errors_cleanly_on_a_non_gzip_file_named_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let gz_path = tmp.path().join("not-actually-gzip.txt.gz");
        std::fs::write(&gz_path, b"this is not gzip data").unwrap();
        assert!(CompressedParser.parse(&gz_path).is_err());
    }
}
