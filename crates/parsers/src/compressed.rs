//! Transparent compressed content indexing (4.5, ripgrep `-z`, extended to zstd/xz/lzma/brotli)
//! — `README.md.gz`, rotated `.log.gz`, man pages get their DECOMPRESSED content indexed
//! instead of being treated as an opaque binary blob. Four codecs, all pure Rust (no C
//! toolchain, no openssl-sys): gzip (`flate2`), zstd (`ruzstd`), xz/lzma (`lzma-rs`), and
//! brotli (`brotli`). Default OFF (`[parsers] compressed = false`, enabled via
//! `Registry::enable_compressed`). `.tar.<codec>` (and its short aliases — `.tgz`, `.tzst`,
//! `.txz`) stay with `archive.rs` — this parser explicitly excludes them so it never hijacks
//! that format, regardless of codec.

use crate::registry::Registry;
use crate::types::{ChunkParams, Extracted, Parser, MAX_ZIP_ENTRY_BYTES};
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use std::io::{BufReader, Read, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Gz,
    Zstd,
    Xz,
    Lzma,
    Brotli,
}

/// `(outer extension, codec, short archive aliases to exclude alongside the plain
/// `.tar.<extension>` form)` — e.g. `.tar.gz`/`.tgz` are both archive.rs's job, not this
/// parser's, for every codec that has a conventional short alias.
const CODECS: &[(&str, Codec, &[&str])] = &[
    ("gz", Codec::Gz, &["tgz"]),
    ("zst", Codec::Zstd, &["tzst"]),
    ("xz", Codec::Xz, &["txz"]),
    ("lzma", Codec::Lzma, &[]),
    ("br", Codec::Brotli, &[]),
];

pub struct CompressedParser;

impl CompressedParser {
    /// The filename a compressed file's DECOMPRESSED content would have — `foo.log.gz` ->
    /// `foo.log` — so it can be routed through the normal registry dispatch (extension/MIME
    /// based), including 1.3's encoding handling for text. `None` for anything that isn't a
    /// standalone compressed file recognized by [`CODECS`] (no matching suffix, or a
    /// `.tar.<codec>`/short-alias archive — `archive.rs`'s job).
    fn inner_name(path: &Path) -> Option<(Codec, String)> {
        let name = path.file_name()?.to_str()?;
        let lower = name.to_ascii_lowercase();
        for &(ext, codec, aliases) in CODECS {
            let suffix = format!(".{ext}");
            if !lower.ends_with(&suffix) {
                continue;
            }
            let is_tar_archive = lower.ends_with(&format!(".tar{suffix}"))
                || aliases.iter().any(|a| lower.ends_with(&format!(".{a}")));
            if is_tar_archive {
                return None;
            }
            return Some((codec, name[..name.len() - suffix.len()].to_owned()));
        }
        None
    }

    /// Decompress `path` (already known to be `codec`) into memory, bomb-guarded at
    /// [`MAX_ZIP_ENTRY_BYTES`] the same way for every codec — either by capping the READ side
    /// (the three `Read`-adapter codecs: gzip/zstd/brotli, via `.take(cap + 1)`) or the WRITE
    /// side (`xz`/`lzma`, whose decompress functions write to completion in one call rather
    /// than lazily — [`CappedWriter`] aborts the call early once the cap would be exceeded).
    fn decompress(codec: Codec, path: &Path) -> Result<Vec<u8>> {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut buf = Vec::new();
        match codec {
            Codec::Gz => {
                GzDecoder::new(file)
                    .take(MAX_ZIP_ENTRY_BYTES + 1)
                    .read_to_end(&mut buf)
                    .with_context(|| format!("decompressing {}", path.display()))?;
            }
            Codec::Zstd => {
                let decoder = ruzstd::decoding::StreamingDecoder::new(file)
                    .with_context(|| format!("initializing zstd decoder for {}", path.display()))?;
                decoder
                    .take(MAX_ZIP_ENTRY_BYTES + 1)
                    .read_to_end(&mut buf)
                    .with_context(|| format!("decompressing {}", path.display()))?;
            }
            Codec::Brotli => {
                brotli::Decompressor::new(file, 4096)
                    .take(MAX_ZIP_ENTRY_BYTES + 1)
                    .read_to_end(&mut buf)
                    .with_context(|| format!("decompressing {}", path.display()))?;
            }
            Codec::Xz | Codec::Lzma => {
                let mut reader = BufReader::new(file);
                let mut capped = CappedWriter {
                    buf: &mut buf,
                    cap: MAX_ZIP_ENTRY_BYTES,
                };
                let result = if codec == Codec::Xz {
                    lzma_rs::xz_decompress(&mut reader, &mut capped)
                } else {
                    lzma_rs::lzma_decompress(&mut reader, &mut capped)
                };
                result.with_context(|| format!("decompressing {}", path.display()))?;
            }
        }
        if buf.len() as u64 > MAX_ZIP_ENTRY_BYTES {
            bail!(
                "{} decompresses to more than the {MAX_ZIP_ENTRY_BYTES}-byte cap — skipping \
                 content parse (metadata-only)",
                path.display()
            );
        }
        Ok(buf)
    }
}

/// A `Write` sink that errors once the total bytes written would exceed `cap` — the bomb-guard
/// for the two codecs (`xz`/`lzma`) whose decompress functions write to completion in one call
/// rather than being a lazy `Read` adapter that `.take(cap)` can bound from the outside.
struct CappedWriter<'a> {
    buf: &'a mut Vec<u8>,
    cap: u64,
}

impl Write for CappedWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let would_be = self.buf.len() as u64 + data.len() as u64;
        if would_be > self.cap {
            return Err(std::io::Error::other(format!(
                "decompressed content exceeds the {}-byte cap",
                self.cap
            )));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Parser for CompressedParser {
    fn accepts_path(&self, path: &Path) -> bool {
        Self::inner_name(path).is_some()
    }

    // Matched by name only — a compressed file's own MIME (application/gzip, application/zstd,
    // …) tells us nothing about what's inside it.
    fn accepts_mime(&self, _mime: &str) -> bool {
        false
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let (codec, inner_name) = Self::inner_name(path)
            .with_context(|| format!("{} is not a recognized compressed file", path.display()))?;

        let buf = Self::decompress(codec, path)?;

        // Write the decompressed bytes to a scratch temp file named with the INNER
        // extension, so the normal registry dispatch routes it to the right native parser
        // purely by extension/MIME — parsers in this codebase read from a real path, they
        // don't accept pre-loaded bytes, so this is the natural integration point rather
        // than a new in-memory Parser API.
        let tmp_dir =
            tempfile::tempdir().context("creating a scratch dir for compressed decompression")?;
        let tmp_path = tmp_dir.path().join(&inner_name);
        std::fs::write(&tmp_path, &buf)
            .with_context(|| format!("writing decompressed content to {}", tmp_path.display()))?;

        // A fresh built-ins-only registry (no `CompressedParser` itself — double-compression
        // isn't recursively unwrapped, matching the "singly-compressed" scope) with the same
        // chunk sizing as the caller's registry.
        let mut extracted = Registry::with_chunk(chunk)
            .parse(&tmp_path)
            .with_context(|| format!("parsing decompressed content of {}", path.display()))?;

        // Report the ORIGINAL compressed path as the source everywhere — the scratch temp path
        // is about to be deleted, and every downstream consumer (store writes, code-graph
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

    fn gzip_bytes(content: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content).unwrap();
        encoder.finish().unwrap()
    }

    fn zstd_bytes(content: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(content, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn xz_bytes(content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(content), &mut out).unwrap();
        out
    }

    fn lzma_bytes(content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::lzma_compress(&mut std::io::Cursor::new(content), &mut out).unwrap();
        out
    }

    fn brotli_bytes(content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        brotli::BrotliCompress(
            &mut std::io::Cursor::new(content),
            &mut out,
            &brotli::enc::BrotliEncoderParams::default(),
        )
        .unwrap();
        out
    }

    type Encoder = fn(&[u8]) -> Vec<u8>;

    /// `(outer extension, encoder fn)` — drives every codec-parameterized test below so each
    /// new codec is exercised identically without duplicating the test bodies five times.
    fn encoders() -> Vec<(&'static str, Encoder)> {
        vec![
            ("gz", gzip_bytes as Encoder),
            ("zst", zstd_bytes),
            ("xz", xz_bytes),
            ("lzma", lzma_bytes),
            ("br", brotli_bytes),
        ]
    }

    #[test]
    fn accepts_path_matches_every_codec_but_not_their_tar_archive_forms() {
        let p = CompressedParser;
        for (ext, _) in encoders() {
            assert!(
                p.accepts_path(Path::new(&format!("README.md.{ext}"))),
                "{ext} should be accepted"
            );
            assert!(
                !p.accepts_path(Path::new(&format!("archive.tar.{ext}"))),
                ".tar.{ext} must stay with archive.rs"
            );
        }
        assert!(!p.accepts_path(Path::new("archive.tgz")));
        assert!(!p.accepts_path(Path::new("archive.tzst")));
        assert!(!p.accepts_path(Path::new("archive.txz")));
        assert!(!p.accepts_path(Path::new("plain.txt")));
        assert!(!p.accepts_mime("application/gzip"));
    }

    #[test]
    fn inner_name_strips_only_the_trailing_codec_extension() {
        for (ext, _) in encoders() {
            assert_eq!(
                CompressedParser::inner_name(Path::new(&format!("access.log.{ext}")))
                    .map(|(_, n)| n),
                Some("access.log".to_owned()),
                "{ext}"
            );
            assert_eq!(
                CompressedParser::inner_name(Path::new(&format!("archive.tar.{ext}"))),
                None,
                "{ext}"
            );
        }
        assert_eq!(CompressedParser::inner_name(Path::new("archive.tgz")), None);
        assert_eq!(
            CompressedParser::inner_name(Path::new("archive.tzst")),
            None
        );
        assert_eq!(CompressedParser::inner_name(Path::new("archive.txz")), None);
    }

    #[test]
    fn parse_decompresses_and_routes_through_the_inner_extension_for_every_codec() {
        for (ext, encode) in encoders() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(format!("notes.md.{ext}"));
            std::fs::write(&path, encode(b"# Heading\n\nSome body text.")).unwrap();

            let extracted = CompressedParser.parse(&path).unwrap_or_else(|e| {
                panic!("{ext} failed to parse: {e:#}");
            });
            assert!(!extracted.chunks.is_empty(), "{ext}");
            assert!(
                extracted
                    .chunks
                    .iter()
                    .any(|c| c.text.contains("Some body text.")),
                "{ext}"
            );
            // Source is reported as the ORIGINAL compressed path, not the scratch temp file.
            assert_eq!(extracted.source, path, "{ext}");
            for c in &extracted.chunks {
                assert_eq!(c.source, path, "{ext}");
            }
        }
    }

    #[test]
    fn parse_rejects_a_decompression_bomb_over_the_cap_for_every_codec() {
        // Highly compressible content that decompresses well past the cap.
        let huge = vec![b'a'; (MAX_ZIP_ENTRY_BYTES + 1024) as usize];
        for (ext, encode) in encoders() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(format!("bomb.txt.{ext}"));
            std::fs::write(&path, encode(&huge)).unwrap();

            let result = CompressedParser.parse(&path);
            assert!(
                result.is_err(),
                "{ext} must refuse to fully decompress an oversized payload"
            );
        }
    }

    #[test]
    fn parse_errors_cleanly_on_a_non_compressed_file_named_with_a_codec_extension() {
        for (ext, _) in encoders() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(format!("not-actually-{ext}.txt.{ext}"));
            std::fs::write(&path, b"this is not compressed data").unwrap();
            assert!(
                CompressedParser.parse(&path).is_err(),
                "{ext} should error on bogus content"
            );
        }
    }
}
