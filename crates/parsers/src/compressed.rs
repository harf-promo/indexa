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
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::MultiGzDecoder;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
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
    /// [`MAX_ZIP_ENTRY_BYTES`] — but the mechanism differs by codec, because `lzma-rs`'s two
    /// decompress functions don't behave alike:
    ///
    /// - gzip/zstd/brotli are lazy `Read` adapters: `.take(cap + 1)` bounds them from the
    ///   outside, so decompression itself never produces more than `cap + 1` bytes.
    /// - `.lzma` decodes into a bounded circular dictionary buffer whose growth is checked
    ///   incrementally — `lzma_decompress_with_options`'s `memlimit` genuinely stops the
    ///   allocation early, not just the final output.
    /// - `.xz` has NO such option in this crate's public API (`xz_decompress` takes no
    ///   options): its block decoder materializes an entire block into a local buffer and
    ///   only hands it to our `CappedWriter` in one `write_all` call afterward — by which
    ///   point a bomb's allocation has already happened, regardless of the writer-side cap.
    ///   [`xz_declared_uncompressed_size`] pre-checks the stream's own Index (a standard part
    ///   of the XZ container, there so tools can get total sizes without decompressing) and
    ///   rejects before ever calling the decoder. This trusts the Index as metadata, not a
    ///   decode-time guarantee — a stream hand-crafted with a mismatched index could still
    ///   bypass it — but it closes the case any standards-compliant encoder produces, which
    ///   covers realistic attack tooling; `CappedWriter` remains as defense-in-depth on top.
    fn decompress(codec: Codec, path: &Path) -> Result<Vec<u8>> {
        if codec == Codec::Xz {
            let declared = xz_declared_uncompressed_size(path)
                .with_context(|| format!("reading XZ index of {}", path.display()))?;
            if declared > MAX_ZIP_ENTRY_BYTES {
                bail!(
                    "{} declares {declared} byte(s) of uncompressed content — more than the \
                     {MAX_ZIP_ENTRY_BYTES}-byte cap — refusing to decompress (metadata-only)",
                    path.display()
                );
            }
        }
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut buf = Vec::new();
        match codec {
            Codec::Gz => {
                // `MultiGzDecoder`, not `GzDecoder`: a single-member decoder silently stops
                // after the FIRST gzip member with no error, dropping the rest of a
                // concatenated multi-member file (`cat a.gz b.gz > c.gz`) — exactly what
                // log-rotation tooling produces, and this module's own doc comment names
                // rotated `.log.gz` as a primary target. `MultiGzDecoder` decodes every
                // member in sequence, same `Read` interface, same cap below.
                MultiGzDecoder::new(file)
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
            Codec::Xz => {
                let mut reader = BufReader::new(file);
                let mut capped = CappedWriter {
                    buf: &mut buf,
                    cap: MAX_ZIP_ENTRY_BYTES,
                };
                lzma_rs::xz_decompress(&mut reader, &mut capped)
                    .with_context(|| format!("decompressing {}", path.display()))?;
            }
            Codec::Lzma => {
                let mut reader = BufReader::new(file);
                let mut capped = CappedWriter {
                    buf: &mut buf,
                    cap: MAX_ZIP_ENTRY_BYTES,
                };
                let options = lzma_rs::decompress::Options {
                    memlimit: Some(MAX_ZIP_ENTRY_BYTES as usize),
                    ..Default::default()
                };
                lzma_rs::lzma_decompress_with_options(&mut reader, &mut capped, &options)
                    .with_context(|| format!("decompressing {}", path.display()))?;
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

/// Decode an XZ "multi-byte integer" (VLI): little-endian base-128 varint, high bit of each
/// byte set iff another byte follows. Up to 9 bytes, matching the format's own encoder/decoder
/// (`lzma_rs::decode::xz::get_multibyte`) — this crate can't call that directly (private), so
/// this is the same, independently-implemented format, only used for the Index we pre-read
/// ourselves rather than through `lzma_rs`.
fn read_xz_vli(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    for i in 0..9u32 {
        let byte = *buf
            .get(*pos)
            .ok_or_else(|| anyhow!("truncated XZ index (VLI ran past the index field)"))?;
        *pos += 1;
        value |= ((byte & 0x7F) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("XZ index VLI exceeds 9 bytes — malformed stream")
}

/// Read the total DECLARED uncompressed size across every block in an XZ stream, from its
/// trailing Index — without decompressing anything. The Index is a standard XZ container
/// field (immediately before the 12-byte Stream Footer) that real tools (`xz --list`) already
/// rely on for exactly this: sizes without a full decode. Format (all integers little-endian):
///
/// - Stream Footer (last 12 bytes): CRC32(4) | Backward Size(4) | Stream Flags(2) | "YZ"(2).
///   `real_index_size = (backward_size + 1) * 4`.
/// - Index (the `real_index_size` bytes immediately before the footer): `0x00` indicator(1) |
///   VLI num_records | per record: VLI unpadded_size, VLI uncompressed_size | zero padding to
///   4-byte alignment | CRC32(4).
///
/// Only reads the trailing ~tens of bytes via seek, regardless of the file's total size.
fn xz_declared_uncompressed_size(path: &Path) -> Result<u64> {
    const FOOTER_LEN: u64 = 12;
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < FOOTER_LEN {
        bail!("file is shorter than an XZ stream footer");
    }

    let mut footer = [0u8; FOOTER_LEN as usize];
    file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    file.read_exact(&mut footer)?;
    if footer[10..12] != [0x59, 0x5A] {
        bail!("missing XZ footer magic bytes");
    }
    let backward_size = u32::from_le_bytes(footer[4..8].try_into().unwrap());
    let index_size = (backward_size as u64 + 1) * 4;
    if index_size > file_len - FOOTER_LEN {
        bail!("declared index size exceeds the file's own length");
    }

    let mut index_buf = vec![0u8; index_size as usize];
    file.seek(SeekFrom::End(-((FOOTER_LEN + index_size) as i64)))?;
    file.read_exact(&mut index_buf)?;
    if index_buf.first() != Some(&0u8) {
        bail!("missing XZ index indicator byte");
    }

    let mut pos = 1usize;
    let num_records = read_xz_vli(&index_buf, &mut pos)?;
    let mut total: u64 = 0;
    for _ in 0..num_records {
        let _unpadded_size = read_xz_vli(&index_buf, &mut pos)?;
        let uncompressed_size = read_xz_vli(&index_buf, &mut pos)?;
        total = total
            .checked_add(uncompressed_size)
            .ok_or_else(|| anyhow!("XZ index declares an overflowing total size"))?;
    }
    Ok(total)
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

    /// Regression test: a `cat a.gz b.gz > combined.gz`-style multi-member gzip file (exactly
    /// what log-rotation tooling produces for `.log.gz`, this module's own named use case) must
    /// have BOTH members' content indexed, not just the first — a single-member `GzDecoder`
    /// stops cleanly after the first member with no error, silently dropping the rest.
    #[test]
    fn parse_decodes_every_member_of_a_multi_member_gzip_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rotated.log.gz");
        let mut combined = gzip_bytes(b"FIRST-MEMBER-MARKER line one\n");
        combined.extend(gzip_bytes(b"SECOND-MEMBER-MARKER line two\n"));
        std::fs::write(&path, &combined).unwrap();

        let extracted = CompressedParser.parse(&path).unwrap();
        let full_text: String = extracted
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            full_text.contains("FIRST-MEMBER-MARKER"),
            "first gzip member must still be indexed: {full_text:?}"
        );
        assert!(
            full_text.contains("SECOND-MEMBER-MARKER"),
            "second gzip member must ALSO be indexed, not silently dropped: {full_text:?}"
        );
    }

    #[test]
    fn xz_declared_uncompressed_size_matches_the_real_payload_length() {
        // Round-trip: the Index we hand-parse must recover the exact uncompressed length,
        // cross-checked against lzma-rs's own encoder (which writes the true size — this
        // proves our independent VLI/footer/index reader is format-compatible).
        for len in [0usize, 1, 273, 4096, 500_000] {
            let content = vec![b'q'; len];
            let compressed = xz_bytes(&content);
            let tmp = tempfile::NamedTempFile::with_suffix(".xz").unwrap();
            std::fs::write(tmp.path(), &compressed).unwrap();
            assert_eq!(
                xz_declared_uncompressed_size(tmp.path()).unwrap(),
                len as u64,
                "declared size must match the real payload length ({len} bytes)"
            );
        }
    }

    #[test]
    fn xz_bomb_is_rejected_via_the_declared_size_before_any_decompression() {
        // C7: distinguishes the NEW pre-check (reads the trailing Index, rejects before
        // `xz_decompress` is ever called) from the OLD post-decode CappedWriter check (which
        // can't fire until an entire block has already been materialized in RAM — see
        // `decompress`'s doc). The error message names come from different code paths;
        // asserting on it proves which one actually fired, not just that *some* rejection
        // happened (the existing over-the-cap test already covers that more weakly).
        let huge = vec![b'a'; (MAX_ZIP_ENTRY_BYTES + 1024) as usize];
        let tmp = tempfile::NamedTempFile::with_suffix(".txt.xz").unwrap();
        std::fs::write(tmp.path(), xz_bytes(&huge)).unwrap();

        let err = CompressedParser.parse(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("declares") && msg.contains("refusing to decompress"),
            "must be rejected by the pre-decode declared-size check, got: {msg}"
        );
    }

    #[test]
    fn lzma_memlimit_rejects_before_exceeding_the_limit() {
        // Direct proof of the mechanism the `Codec::Lzma` arm of `decompress` relies on:
        // `memlimit` bounds the LZMA decoder's internal circular buffer AS IT GROWS (verified
        // by reading lzma-rs's own source — `LzCircularBuffer::set` checks `new_len <=
        // self.memlimit` before every resize), not just the final output length. This is the
        // actual guard against a maliciously oversized `dict_size` header value (e.g.
        // 0xFFFFFFFF), which would otherwise let the decoder try to grow the buffer toward
        // 4 GiB regardless of how small the real compressed payload is.
        let content = vec![b'z'; 5000]; // enough real back-references to grow well past a tiny memlimit
        let compressed = lzma_bytes(&content);
        let mut reader = std::io::Cursor::new(compressed);
        let mut output = Vec::new();
        let options = lzma_rs::decompress::Options {
            memlimit: Some(100), // far below the 5000-byte content
            ..Default::default()
        };
        let result = lzma_rs::lzma_decompress_with_options(&mut reader, &mut output, &options);
        assert!(
            result.is_err(),
            "memlimit=100 must reject content that needs a bigger buffer"
        );
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
