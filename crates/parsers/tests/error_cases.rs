//! Error-path contract for the parser layer: adversarial / malformed input must
//! degrade gracefully — a content parser returns a stub (never panics), a
//! container parser fails cleanly with `Err`, and `parse_guarded` never panics
//! and honours the size cap. One bad file must never abort a whole scan.

use indexa_parsers::registry::{parse, parse_guarded};
use std::path::Path;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn malformed_pdf_degrades_to_a_stub() {
    // A file that announces itself as a PDF but is garbage past the header. The
    // PDF parser must catch the extractor's failure and return a filename stub,
    // not error or panic (mirrors the inline pdf_parser_handles_corrupt_gracefully).
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        dir.path(),
        "broken.pdf",
        b"%PDF-1.4\n%garbage not a real pdf body\n",
    );
    let ex = parse(&p).expect("malformed PDF should degrade to a stub, not Err");
    assert!(
        !ex.chunks.is_empty(),
        "a corrupt PDF must still yield at least a filename stub chunk"
    );
}

#[test]
fn garbage_image_degrades_to_a_stub() {
    // Random bytes with an image extension: EXIF parsing fails, the parser falls
    // back to a filename stub rather than erroring.
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        dir.path(),
        "not-really.png",
        &[0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    );
    let ex = parse(&p).expect("garbage image should degrade to a stub, not Err");
    assert!(!ex.chunks.is_empty());
}

#[test]
fn truncated_media_degrades_to_a_stub() {
    // ffprobe is absent on CI (and would reject this truncated file anyway); the
    // media parser must still return a stub describing the file — always Ok.
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "clip.mp3", b"ID3\x03\x00truncated");
    let ex = parse(&p).expect("truncated media should degrade to a stub, not Err");
    assert_eq!(ex.chunks.len(), 1);
    assert!(!ex.chunks[0].text.is_empty());
}

#[test]
fn malformed_epub_fails_cleanly_without_panicking() {
    // EPUB is a ZIP container — random bytes aren't a valid archive. The parser
    // must surface a clean Err (not panic). parse_guarded's catch_unwind would
    // convert any internal panic into Err, so a non-panicking Err here proves the
    // failure was graceful at the source.
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        dir.path(),
        "broken.epub",
        b"this is definitely not a zip archive",
    );
    let direct = parse(&p);
    assert!(direct.is_err(), "a non-zip .epub must Err, got {direct:?}");
    let size = std::fs::metadata(&p).unwrap().len();
    // The guarded path returns the same Err — and crucially does not panic.
    assert!(parse_guarded(&p, size, 0).is_err());
}

#[test]
fn parse_guarded_skips_files_over_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "big.txt", b"some real content here");
    let size = std::fs::metadata(&p).unwrap().len();
    // A cap below the file size → skipped without reading.
    assert!(parse_guarded(&p, size, 1).is_err());
    // 0 disables the cap; a generous cap parses fine.
    assert!(parse_guarded(&p, size, 0).is_ok());
    assert!(parse_guarded(&p, size, 10_000_000).is_ok());
}

#[test]
fn empty_file_is_handled_gracefully() {
    // A zero-byte file with an unknown name sniffs as (empty) text rather than
    // bailing — and never panics.
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "EMPTY", b"");
    let ex = parse(&p).expect("empty file should parse as empty text");
    // No content ⇒ no chunks; the contract is "handled", not "non-empty".
    assert!(ex.chunks.is_empty());
}
