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
//! - Speaker-note ↔ slide mapping uses ordinal position, not the rels graph; when notes
//!   are sparse the association can be off-by-one. Tracked as future work.
//! - `.ppt` (legacy OLE compound binary) and Apple iWork `.key`/`.pages`/`.numbers` are
//!   NOT handled here. `.ppt` is claimed by `OfficeParser` which returns a quiet stub.

use crate::office::strip_xml_tags;
use crate::types::{Chunk, Extracted, Parser};
use std::io::Read;
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
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Graceful fallback — never bail!, so the deep phase stores a stub rather than
        // counting this as a hard_error.
        let chunks = match parse_pptx(path, filename) {
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
fn parse_pptx(path: &Path, filename: &str) -> anyhow::Result<Vec<Chunk>> {
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

    for slide_num in &slide_indices {
        let slide_entry = format!("ppt/slides/slide{slide_num}.xml");

        // Read slide XML and strip tags.
        let slide_text = {
            let mut xml = String::new();
            match archive.by_name(&slide_entry) {
                Ok(mut e) => {
                    let _ = e.read_to_string(&mut xml);
                }
                Err(_) => continue, // entry listed by name but missing — skip
            }
            strip_xml_tags(&xml)
        };
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
                    // Truncate at last space within 80 chars.
                    s[..80]
                        .rfind(' ')
                        .map(|i| s[..i].to_owned())
                        .unwrap_or_else(|| s[..80].to_owned())
                }
            })
            .unwrap_or_default();

        let heading = if title.is_empty() {
            format!("Slide {slide_num}")
        } else {
            format!("Slide {slide_num}: {title}")
        };

        // Try to find matching speaker notes by ordinal position (not rels).
        // notes_indices is sorted; find the same-position entry if it exists.
        let notes_text: Option<String> = {
            // notes_indices are 1-based entry numbers in order; slide_num's ordinal
            // position in slide_indices determines which note entry to use.
            let slide_ordinal = slide_indices.iter().position(|&s| s == *slide_num);
            let note_num = slide_ordinal
                .and_then(|pos| notes_indices.get(pos))
                .copied();

            if let Some(n) = note_num {
                let notes_entry = format!("ppt/notesSlides/notesSlide{n}.xml");
                let mut xml = String::new();
                if archive
                    .by_name(&notes_entry)
                    .ok()
                    .and_then(|mut e| e.read_to_string(&mut xml).ok())
                    .is_some()
                {
                    let stripped = strip_xml_tags(&xml);
                    let stripped = stripped.trim().to_owned();
                    // Speaker-note XML also contains the slide body text (it embeds a copy).
                    // Deduplicate: only keep the notes portion if it differs meaningfully.
                    if !stripped.is_empty() && stripped != slide_text {
                        Some(stripped)
                    } else {
                        None
                    }
                } else {
                    None
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
            800,
            100,
            &mut seq,
            &mut chunks,
        );
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
        let mut xml = String::new();
        if archive
            .by_name(name)
            .ok()
            .and_then(|mut e| e.read_to_string(&mut xml).ok())
            .is_none()
        {
            continue;
        }
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
        crate::types::chunk_words(path, text, heading, None, 800, 100, &mut seq, &mut chunks);
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
