//! OOXML Presentation parser (.pptx / .ppsx) — extracts per-slide text and speaker notes.
//!
//! PPTX files are ZIP archives. Visible slide text lives in `ppt/slides/slideN.xml`
//! inside `<a:t>` runs; speaker notes in `ppt/notesSlides/notesSlideN.xml`. Slides
//! are enumerated by parsing the integer suffix from entry names and sorting numerically
//! (never assume contiguous or lexical ordering).
//!
//! # Limitations (known, documented)
//! - Chart text (`ppt/charts/chartN.xml`) and SmartArt node text (`ppt/diagrams/dataN.xml`)
//!   ARE extracted as deck-level chunks (v0.54) — mapping a chart back to its slide needs the
//!   rels graph, so they're emitted deck-level. Embedded OLE objects and the chart/diagram
//!   styling parts (`colorsN`/`layoutN`/`styleN`) are still skipped.
//! - Slide-master / slide-layout placeholder text is intentionally skipped (template boilerplate).
//! - Speaker-note ↔ slide mapping follows the rels graph
//!   (`ppt/slides/_rels/slideN.xml.rels` → `notesSlideM.xml`), so it's correct even when
//!   only some slides have notes. Files lacking a rels part fall back to ordinal position
//!   only when notes and slides are 1:1 (otherwise no note is attached, never a wrong one).
//! - `.ppt` (legacy OLE compound binary) and Apple iWork `.key`/`.pages`/`.numbers` are
//!   NOT handled here. `.ppt` is claimed by `OfficeParser` which returns a quiet stub.

use crate::office::strip_xml_tags;
use crate::types::{Chunk, ChunkParams, Extracted, Parser};
use std::path::Path;

pub struct PresentationParser;

impl Parser for PresentationParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pptx" | "ppsx")
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        matches!(
            mime,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
        )
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[("pptx", Full), ("ppsx", Full)]
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> anyhow::Result<Extracted> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Graceful fallback — never bail!, so the deep phase stores a stub rather than
        // counting this as a hard_error.
        let chunks = match parse_pptx(path, filename, chunk) {
            Ok(c) if !c.is_empty() => c,
            _ => fallback_chunk(path, filename),
        };

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                .to_owned(),
            chunks,
            edges: Vec::new(),
        })
    }
}

// ── Internal extraction ───────────────────────────────────────────────────────

fn fallback_chunk(path: &Path, filename: &str) -> Vec<Chunk> {
    vec![Chunk {
        source: path.to_path_buf(),
        seq: 0,
        heading: String::new(),
        text: format!("Presentation: {filename}"),
        language: None,
    }]
}

/// Extract slide text + notes from a PPTX zip. Returns `Err` only on I/O failure
/// opening the file; individual slide errors are silently skipped (best-effort).
fn parse_pptx(path: &Path, filename: &str, chunk: ChunkParams) -> anyhow::Result<Vec<Chunk>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Collect all zip entry names once (can't hold the borrow across by_name calls).
    let all_names: Vec<String> = archive.file_names().map(|s| s.to_owned()).collect();

    // --- Enumerate slides numerically -----------------------------------------
    // Entry format: ppt/slides/slideN.xml  (N is a positive integer, may be non-contiguous).
    let mut slide_indices: Vec<u32> = all_names
        .iter()
        .filter_map(|name| slide_index(name))
        .collect();
    slide_indices.sort_unstable();
    slide_indices.dedup();

    if slide_indices.is_empty() {
        // No slides found — return fallback so the caller emits a stub.
        return Ok(fallback_chunk(path, filename));
    }

    // --- Enumerate notes numerically ------------------------------------------
    let mut notes_indices: Vec<u32> = all_names
        .iter()
        .filter_map(|name| notes_index(name))
        .collect();
    notes_indices.sort_unstable();
    notes_indices.dedup();

    let mut chunks = Vec::new();
    let mut seq = 0usize;
    // Running total across slides + notes: a deck of many near-cap parts can't sum to an OOM.
    let mut extracted_bytes: u64 = 0;

    for slide_num in &slide_indices {
        let slide_entry = format!("ppt/slides/slide{slide_num}.xml");

        // Read slide XML (capped) and strip tags.
        let Some(slide_xml) = read_zip_text(&mut archive, &slide_entry) else {
            continue; // entry listed by name but missing/unreadable — skip
        };
        extracted_bytes = extracted_bytes.saturating_add(slide_xml.len() as u64);
        let slide_text = strip_xml_tags(&slide_xml);
        let slide_text = slide_text.trim().to_owned();

        // Derive slide title: first non-empty whitespace-collapsed token sequence
        // that is ≤ 80 chars (heuristic — slide titles are usually the first run).
        let title: String = slide_text
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| {
                let s = l.trim();
                if s.len() <= 80 {
                    s.to_owned()
                } else {
                    // Truncate near 80, preferring the last space — but only on a UTF-8 char
                    // boundary, so a multibyte glyph straddling byte 80 never panics.
                    let mut end = 80;
                    while end > 0 && !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    let head = &s[..end];
                    head.rfind(' ')
                        .map(|i| head[..i].to_owned())
                        .unwrap_or_else(|| head.to_owned())
                }
            })
            .unwrap_or_default();

        let heading = if title.is_empty() {
            format!("Slide {slide_num}")
        } else {
            format!("Slide {slide_num}: {title}")
        };

        // Map this slide to its speaker-notes part via the rels graph
        // (`ppt/slides/_rels/slideN.xml.rels` → `notesSlideM.xml`), which is correct even
        // when notes are sparse. Fall back to ordinal position ONLY when notes and slides
        // are 1:1 (the count matches), where ordinal is safe; otherwise no note.
        let notes_text: Option<String> = {
            let rels_entry = format!("ppt/slides/_rels/slide{slide_num}.xml.rels");
            let note_num = read_zip_text(&mut archive, &rels_entry)
                .as_deref()
                .and_then(notes_target_from_rels)
                .or_else(|| {
                    if notes_indices.len() == slide_indices.len() {
                        slide_indices
                            .iter()
                            .position(|&s| s == *slide_num)
                            .and_then(|pos| notes_indices.get(pos))
                            .copied()
                    } else {
                        None
                    }
                });

            if let Some(n) = note_num {
                let notes_entry = format!("ppt/notesSlides/notesSlide{n}.xml");
                match read_zip_text(&mut archive, &notes_entry) {
                    Some(xml) => {
                        let stripped = strip_xml_tags(&xml);
                        let stripped = stripped.trim().to_owned();
                        // Speaker-note XML also contains the slide body text (it embeds a copy).
                        // Deduplicate: only keep the notes portion if it differs meaningfully.
                        if !stripped.is_empty() && stripped != slide_text {
                            Some(stripped)
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            } else {
                None
            }
        };

        // Combine slide body + notes.
        let full_text = match notes_text {
            Some(ref n) => format!("{slide_text}\n\nSpeaker notes: {n}"),
            None => slide_text.clone(),
        };

        if full_text.trim().is_empty() {
            // Empty slide — skip rather than emitting a noise chunk.
            continue;
        }

        // Emit one chunk per slide; oversized slides split via chunk_words.
        crate::types::chunk_words(
            path,
            &full_text,
            &heading,
            None,
            chunk.size,
            chunk.overlap,
            &mut seq,
            &mut chunks,
        );

        if extracted_bytes > crate::types::MAX_ZIP_TOTAL_BYTES {
            break;
        }
    }

    // Chart + SmartArt text — nested OOXML parts the slide pass skips. Emit them as deck-level
    // chunks (mapping a chart to its slide needs the rels graph; deck-level keeps this
    // dependency-free and sidesteps the notes off-by-one class of bug). Sorted for determinism.
    let mut aux_names: Vec<&String> = all_names
        .iter()
        .filter(|n| is_chart_or_diagram(n))
        .collect();
    aux_names.sort();
    for name in aux_names {
        if extracted_bytes > crate::types::MAX_ZIP_TOTAL_BYTES {
            break;
        }
        let Some(xml) = read_zip_text(&mut archive, name) else {
            continue;
        };
        extracted_bytes = extracted_bytes.saturating_add(xml.len() as u64);
        let text = strip_xml_tags(&xml);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let heading = if name.contains("/charts/") {
            "Chart"
        } else {
            "Diagram"
        };
        crate::types::chunk_words(
            path,
            text,
            heading,
            None,
            chunk.size,
            chunk.overlap,
            &mut seq,
            &mut chunks,
        );
    }

    // If all slides were empty/skipped, return fallback.
    if chunks.is_empty() {
        return Ok(fallback_chunk(path, filename));
    }

    Ok(chunks)
}

/// A chart (`ppt/charts/chartN.xml`) or SmartArt data-model (`ppt/diagrams/dataN.xml`) part?
/// Only `dataN.xml` carries SmartArt node text; the `colorsN`/`layoutN`/`quickStyleN`/`drawingN`
/// diagram parts are styling/layout (no user text) and are excluded.
fn is_chart_or_diagram(name: &str) -> bool {
    name.ends_with(".xml")
        && (name.starts_with("ppt/charts/chart") || name.starts_with("ppt/diagrams/data"))
}

/// Extract numeric suffix from `ppt/slides/slideN.xml` entries.
fn slide_index(name: &str) -> Option<u32> {
    let base = name.strip_prefix("ppt/slides/slide")?;
    base.strip_suffix(".xml")?.parse::<u32>().ok()
}

/// Extract numeric suffix from `ppt/notesSlides/notesSlideN.xml` entries.
fn notes_index(name: &str) -> Option<u32> {
    let base = name.strip_prefix("ppt/notesSlides/notesSlide")?;
    base.strip_suffix(".xml")?.parse::<u32>().ok()
}

/// Read a zip entry to a UTF-8 string, capped at [`MAX_ZIP_ENTRY_BYTES`] (best-effort; `None` if
/// absent/unreadable). Central choke point so every PPTX part read is bomb-bounded.
fn read_zip_text(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    crate::types::read_zip_entry_text(
        archive.by_name(name).ok()?,
        crate::types::MAX_ZIP_ENTRY_BYTES,
    )
    .ok()
}

/// Parse a slide's `.rels` XML for its notesSlide target number — the `M` in a
/// `Target="../notesSlides/notesSlideM.xml"` relationship. Scans for `notesSlide`
/// immediately followed by digits (the folder token `notesSlides` is followed by `s`,
/// so it's skipped). Returns the first match, or `None` if the slide has no notes rel.
fn notes_target_from_rels(rels_xml: &str) -> Option<u32> {
    let mut rest = rels_xml;
    while let Some(pos) = rest.find("notesSlide") {
        let after = &rest[pos + "notesSlide".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return digits.parse::<u32>().ok();
        }
        rest = after;
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal PPTX zip in memory.
    /// `slides`: (slide_number, visible_text, optional_notes_text).
    fn build_pptx(slides: &[(u32, &str, Option<&str>)]) -> Vec<u8> {
        use zip::write::FileOptions;
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);

        // Minimal [Content_Types].xml (zip requires at least one entry start).
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"/>").unwrap();

        for (num, text, notes) in slides {
            // Slide XML with visible text in <a:t> runs.
            let slide_xml = format!(
                "<p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"\
                        xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
                  <p:cSld><p:spTree><p:sp><p:txBody>\
                    <a:p><a:r><a:t>{text}</a:t></a:r></a:p>\
                  </p:txBody></p:sp></p:spTree></p:cSld>\
                </p:sld>"
            );
            zip.start_file(format!("ppt/slides/slide{num}.xml"), opts)
                .unwrap();
            zip.write_all(slide_xml.as_bytes()).unwrap();

            if let Some(notes_text) = notes {
                let notes_xml = format!(
                    "<p:notes xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"\
                              xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
                      <p:cSld><p:spTree><p:sp><p:txBody>\
                        <a:p><a:r><a:t>{notes_text}</a:t></a:r></a:p>\
                      </p:txBody></p:sp></p:spTree></p:cSld>\
                    </p:notes>"
                );
                zip.start_file(format!("ppt/notesSlides/notesSlide{num}.xml"), opts)
                    .unwrap();
                zip.write_all(notes_xml.as_bytes()).unwrap();

                // Slide → notes rels (matches a real PPTX), so the rels-based mapping path
                // is what the tests exercise.
                let rels = format!(
                    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
                       <Relationship Id=\"rId1\" \
                         Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide\" \
                         Target=\"../notesSlides/notesSlide{num}.xml\"/>\
                     </Relationships>"
                );
                zip.start_file(format!("ppt/slides/_rels/slide{num}.xml.rels"), opts)
                    .unwrap();
                zip.write_all(rels.as_bytes()).unwrap();
            }
        }

        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn presentation_parser_accepts_pptx_not_docx() {
        let p = PresentationParser;
        assert!(p.accepts_path(Path::new("deck.pptx")));
        assert!(p.accepts_path(Path::new("show.ppsx")));
        assert!(!p.accepts_path(Path::new("doc.docx")));
        assert!(!p.accepts_path(Path::new("slide.ppt")));
        assert!(p.accepts_mime(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        ));
        assert!(!p.accepts_mime(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
    }

    #[test]
    fn pptx_extracts_slides_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deck.pptx");
        std::fs::write(
            &p,
            build_pptx(&[
                (1, "First slide content", None),
                (2, "Second slide content", None),
                (3, "Third slide content", None),
            ]),
        )
        .unwrap();

        let ex = PresentationParser.parse(&p).unwrap();
        assert_eq!(ex.chunks.len(), 3, "expected 3 slide chunks");
        assert_eq!(ex.chunks[0].seq, 0);
        assert!(
            ex.chunks[0].text.contains("First slide"),
            "chunk 0: {}",
            ex.chunks[0].text
        );
        assert!(
            ex.chunks[0].heading.contains("Slide 1"),
            "heading: {}",
            ex.chunks[0].heading
        );
        assert!(ex.chunks[1].text.contains("Second slide"));
        assert!(ex.chunks[2].text.contains("Third slide"));
    }

    #[test]
    fn pptx_extracts_speaker_notes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("noted.pptx");
        std::fs::write(
            &p,
            build_pptx(&[(
                1,
                "Slide body text",
                Some("This is the speaker note for slide 1"),
            )]),
        )
        .unwrap();

        let ex = PresentationParser.parse(&p).unwrap();
        let combined: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            combined.contains("Slide body"),
            "body not found: {combined}"
        );
        assert!(
            combined.contains("speaker note"),
            "notes not found: {combined}"
        );
    }

    #[test]
    fn pptx_non_contiguous_numbering_sorts_numerically() {
        // slide1, slide2, slide10 — lexical order would put 10 before 2.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("numbered.pptx");
        std::fs::write(
            &p,
            build_pptx(&[(1, "Alpha", None), (2, "Beta", None), (10, "Gamma", None)]),
        )
        .unwrap();

        let ex = PresentationParser.parse(&p).unwrap();
        assert_eq!(ex.chunks.len(), 3);
        assert!(
            ex.chunks[0].text.contains("Alpha"),
            "got: {}",
            ex.chunks[0].text
        );
        assert!(
            ex.chunks[1].text.contains("Beta"),
            "got: {}",
            ex.chunks[1].text
        );
        assert!(
            ex.chunks[2].text.contains("Gamma"),
            "got: {}",
            ex.chunks[2].text
        );
        // heading must say Slide 10, not Slide 2 (numeric ordering).
        assert!(
            ex.chunks[2].heading.contains("10"),
            "heading: {}",
            ex.chunks[2].heading
        );
    }

    #[test]
    fn pptx_missing_notes_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nonotes.pptx");
        std::fs::write(
            &p,
            build_pptx(&[(1, "Slide without notes", None), (2, "Another slide", None)]),
        )
        .unwrap();
        // Must not panic and must return slide body text.
        let ex = PresentationParser.parse(&p).unwrap();
        assert_eq!(ex.chunks.len(), 2);
        assert!(ex.chunks[0].text.contains("Slide without notes"));
    }

    #[test]
    fn pptx_empty_slides_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.pptx");
        // One real slide, one with blank text.
        std::fs::write(
            &p,
            build_pptx(&[
                (1, "Real content here", None),
                (2, "   ", None), // whitespace only → strip → empty → skipped
            ]),
        )
        .unwrap();
        let ex = PresentationParser.parse(&p).unwrap();
        // At least the real slide must be present; blank slide may or may not produce a chunk.
        let combined: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(combined.contains("Real content"), "got: {combined}");
    }

    #[test]
    fn pptx_corrupt_zip_falls_back_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("corrupt.pptx");
        std::fs::write(&p, b"this is definitely not a zip file").unwrap();
        // Must return Ok with a fallback chunk, never panic.
        let ex = PresentationParser.parse(&p).unwrap();
        assert!(!ex.chunks.is_empty(), "must have fallback chunk");
        let combined: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            combined.contains("Presentation:"),
            "fallback text expected, got: {combined}"
        );
    }

    #[test]
    fn pptx_extracts_chart_and_diagram_text() {
        use zip::write::FileOptions;
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(b"<?xml version=\"1.0\"?><Types/>").unwrap();
        // One slide so parse_pptx doesn't fall back.
        zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zip.write_all(b"<p:sld xmlns:a=\"a\" xmlns:p=\"p\"><a:t>Quarterly deck</a:t></p:sld>")
            .unwrap();
        // A chart part: title run + a data value.
        zip.start_file("ppt/charts/chart1.xml", opts).unwrap();
        zip.write_all(
            b"<c:chart xmlns:c=\"c\" xmlns:a=\"a\"><c:title><a:t>Revenue by region</a:t>\
              </c:title><c:v>42</c:v></c:chart>",
        )
        .unwrap();
        // A SmartArt data model: node text.
        zip.start_file("ppt/diagrams/data1.xml", opts).unwrap();
        zip.write_all(
            b"<dgm:dataModel xmlns:dgm=\"d\" xmlns:a=\"a\"><a:t>Plan</a:t><a:t>Build</a:t>\
              <a:t>Ship</a:t></dgm:dataModel>",
        )
        .unwrap();
        // A diagram *styling* part — must NOT be extracted (excluded by name; no user text anyway).
        zip.start_file("ppt/diagrams/colors1.xml", opts).unwrap();
        zip.write_all(
            b"<dgm:colors xmlns:dgm=\"d\" xmlns:a=\"a\"><a:srgbClr val=\"FF0000\"/></dgm:colors>",
        )
        .unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("charts.pptx");
        std::fs::write(&p, bytes).unwrap();

        let ex = PresentationParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("Revenue by region"), "chart title: {all}");
        assert!(all.contains("42"), "chart data value: {all}");
        assert!(
            all.contains("Plan") && all.contains("Ship"),
            "SmartArt node text: {all}"
        );
        let headings: String = ex
            .chunks
            .iter()
            .map(|c| c.heading.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(headings.contains("Chart"), "chart heading: {headings}");
        assert!(headings.contains("Diagram"), "diagram heading: {headings}");
    }

    #[test]
    fn pptx_long_multibyte_title_does_not_panic() {
        // A >80-byte first line whose codepoint straddles byte 80 (em-dashes are 3 bytes).
        // The title-truncation must slice on a char boundary, never panic.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mb.pptx");
        let long = "Quarterly review ".to_string() + &"— section ".repeat(12);
        std::fs::write(&p, build_pptx(&[(1, &long, None)])).unwrap();
        let ex = PresentationParser.parse(&p).unwrap(); // must not panic
        assert!(!ex.chunks.is_empty());
    }

    #[test]
    fn notes_target_from_rels_extracts_number() {
        let rels = "<Relationships><Relationship Type=\"…/notesSlide\" \
                    Target=\"../notesSlides/notesSlide3.xml\"/></Relationships>";
        assert_eq!(notes_target_from_rels(rels), Some(3));
        // The folder token `notesSlides` (followed by `/`) must not be mistaken for a target.
        assert_eq!(notes_target_from_rels("just notesSlides/ here"), None);
        assert_eq!(notes_target_from_rels("<Relationships/>"), None);
    }

    #[test]
    fn pptx_sparse_notes_map_to_correct_slide_via_rels() {
        // Only the SECOND slide has a note, stored as notesSlide1.xml and linked by slide2's
        // rels. The old ordinal mapping would mis-attach it to slide 1 (off-by-one); the rels
        // mapping must attach it to slide 2 and leave slide 1 note-free.
        use zip::write::FileOptions;
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        let body = |t: &str| {
            format!(
                "<p:sld xmlns:a=\"a\" xmlns:p=\"p\"><p:txBody><a:p><a:r><a:t>{t}</a:t>\
                     </a:r></a:p></p:txBody></p:sld>"
            )
        };
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(b"<?xml version=\"1.0\"?><Types/>").unwrap();
        zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zip.write_all(body("First slide alpha").as_bytes()).unwrap();
        zip.start_file("ppt/slides/slide2.xml", opts).unwrap();
        zip.write_all(body("Second slide beta").as_bytes()).unwrap();
        // The single note (stored as notesSlide1.xml) belongs to slide 2.
        zip.start_file("ppt/notesSlides/notesSlide1.xml", opts)
            .unwrap();
        zip.write_all(body("NOTE FOR THE SECOND SLIDE").as_bytes())
            .unwrap();
        zip.start_file("ppt/slides/_rels/slide2.xml.rels", opts)
            .unwrap();
        zip.write_all(
            b"<Relationships><Relationship Type=\"x/notesSlide\" \
              Target=\"../notesSlides/notesSlide1.xml\"/></Relationships>",
        )
        .unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sparse.pptx");
        std::fs::write(&p, bytes).unwrap();
        let ex = PresentationParser.parse(&p).unwrap();

        let slide1 = ex.chunks.iter().find(|c| c.text.contains("alpha")).unwrap();
        let slide2 = ex.chunks.iter().find(|c| c.text.contains("beta")).unwrap();
        assert!(
            !slide1.text.contains("NOTE FOR THE SECOND SLIDE"),
            "note must NOT attach to slide 1 (the off-by-one bug): {}",
            slide1.text
        );
        assert!(
            slide2.text.contains("NOTE FOR THE SECOND SLIDE"),
            "note must attach to slide 2 via rels: {}",
            slide2.text
        );
    }

    #[test]
    fn slide_index_parses_correctly() {
        assert_eq!(slide_index("ppt/slides/slide1.xml"), Some(1));
        assert_eq!(slide_index("ppt/slides/slide10.xml"), Some(10));
        assert_eq!(slide_index("ppt/slides/slide99.xml"), Some(99));
        assert_eq!(slide_index("ppt/notesSlides/notesSlide1.xml"), None);
        assert_eq!(slide_index("ppt/slides/slideLayout1.xml"), None);
    }

    #[test]
    fn notes_index_parses_correctly() {
        assert_eq!(notes_index("ppt/notesSlides/notesSlide1.xml"), Some(1));
        assert_eq!(notes_index("ppt/notesSlides/notesSlide10.xml"), Some(10));
        assert_eq!(notes_index("ppt/slides/slide1.xml"), None);
    }
}
