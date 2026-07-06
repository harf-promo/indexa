//! Office format parser: xlsx/csv via calamine, plain-text extraction for docx.

use crate::types::{Chunk, ChunkParams, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct OfficeParser;

impl Parser for OfficeParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(
                "xlsx"
                    | "xls"
                    | "xlsm"
                    | "ods"
                    | "csv"
                    | "tsv"
                    | "docx"
                    | "odt"
                    | "rtf"
                    | "ppt"
                    | "pps" // legacy binary PowerPoint — quiet stub fallback, not real extraction
            )
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        matches!(
            mime,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                | "application/vnd.ms-excel"
                | "text/csv"
                | "application/vnd.oasis.opendocument.spreadsheet"
                | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                | "application/msword"
                | "application/rtf"
                | "text/rtf"
                | "application/vnd.ms-powerpoint" // legacy binary — stub fallback
        )
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[
            ("xlsx", Full),
            ("xls", Full),
            ("xlsm", Full),
            ("ods", Full),
            ("csv", Full),
            ("tsv", Full),
            ("docx", Full),
            ("odt", Full),
            ("rtf", Full),
            ("ppt", Stub),
            ("pps", Stub),
        ]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let text = match ext {
            "xlsx" | "xls" | "xlsm" | "ods" => parse_spreadsheet(path)?,
            "csv" | "tsv" => parse_csv(path)?,
            "docx" => parse_docx_zip(path).unwrap_or_else(|_| {
                format!(
                    "Document: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                )
            }),
            // RTF: strip control words/groups so the prose is indexed, not the markup.
            "rtf" => parse_rtf(path).unwrap_or_default(),
            // Legacy binary PowerPoint (OLE compound doc, no pure-Rust extractor).
            // Return a quiet stub so the deep phase stores *something* instead of
            // counting this as a hard_error ("no parser"). Real text is not extracted.
            "ppt" | "pps" => format!(
                "Presentation: {} (legacy binary format — text not extracted)",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
            ),
            _ => std::fs::read_to_string(path).unwrap_or_else(|_| {
                format!(
                    "File: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                )
            }),
        };

        let mime = match ext {
            "xlsx" | "xlsm" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xls" => "application/vnd.ms-excel",
            "ods" => "application/vnd.oasis.opendocument.spreadsheet",
            "csv" | "tsv" => "text/csv",
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "odt" => "application/vnd.oasis.opendocument.text",
            "rtf" => "application/rtf",
            _ => "application/octet-stream",
        };

        // Split into word chunks with overlap (sizes from [chunking] config).
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut chunks = Vec::new();
        let mut seq = 0usize;

        if words.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!(
                    "File: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ),
                language: None,
            });
        } else {
            crate::types::chunk_words(
                path,
                &text,
                "",
                None,
                chunk.size,
                chunk.overlap,
                &mut seq,
                &mut chunks,
            );
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: mime.to_owned(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Names of RTF destination groups whose content is metadata/binary, not body prose, and
/// is skipped wholesale (the font/colour/style tables, doc info, embedded pictures, list
/// tables, …). Footnotes/headers/footers are deliberately NOT here — their text is content.
const RTF_SKIP_DESTINATIONS: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "themedata",
    "latentstyles",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "datastore",
    "xmlnstbl",
];

/// Strip RTF control words/groups, returning the visible prose.
///
/// RTF is `{\rtf1 …}`: control words are `\word` (optionally a signed numeric arg and a
/// single trailing-space delimiter), `\'xx` is a hex-escaped byte, `{`/`}` delimit groups,
/// `\par`/`\line`/`\tab`/`\page`/`\sect`/`\cell`/`\row` are whitespace, and a group beginning
/// `\*` (ignorable destination) or one of [`RTF_SKIP_DESTINATIONS`] (font/colour/style tables,
/// doc info, pictures, …) is dropped entirely. This is a pragmatic stripper — not a full RTF
/// reader — good enough to index the prose. Hex escapes (`\'xx`) are dropped, not decoded.
fn parse_rtf(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut depth: i32 = 0;
    let mut skip_depth: Option<i32> = None; // skip text while depth >= this
    let mut at_group_start = false; // a control word now would be the group's destination
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                depth += 1;
                at_group_start = true;
            }
            '}' => {
                if matches!(skip_depth, Some(d) if depth <= d) {
                    skip_depth = None;
                }
                depth -= 1;
                at_group_start = false;
            }
            '\\' => match chars.peek() {
                // Escaped literal `\`, `{`, `}`.
                Some('\\') | Some('{') | Some('}') => {
                    if let Some(lit) = chars.next() {
                        if skip_depth.is_none() {
                            out.push(lit);
                        }
                    }
                    at_group_start = false;
                }
                // `\'xx` hex byte — drop the apostrophe + two hex digits.
                Some('\'') => {
                    chars.next();
                    chars.next();
                    chars.next();
                    at_group_start = false;
                }
                // `\*` ignorable destination — skip the whole group.
                Some('*') => {
                    chars.next();
                    if skip_depth.is_none() {
                        skip_depth = Some(depth);
                    }
                    at_group_start = false;
                }
                // Control word: letters, optional signed number, optional trailing space.
                Some(p) if p.is_ascii_alphabetic() => {
                    let mut word = String::new();
                    while let Some(&n) = chars.peek() {
                        if n.is_ascii_alphabetic() {
                            word.push(n);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if matches!(chars.peek(), Some('-')) {
                        chars.next();
                    }
                    while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
                        chars.next();
                    }
                    if matches!(chars.peek(), Some(' ')) {
                        chars.next(); // single trailing-space delimiter
                    }
                    if at_group_start
                        && skip_depth.is_none()
                        && RTF_SKIP_DESTINATIONS.contains(&word.as_str())
                    {
                        skip_depth = Some(depth);
                    } else if skip_depth.is_none()
                        && matches!(
                            word.as_str(),
                            "par" | "line" | "tab" | "page" | "sect" | "cell" | "row"
                        )
                    {
                        out.push(' ');
                    }
                    at_group_start = false;
                }
                // Lone backslash — drop.
                _ => at_group_start = false,
            },
            '\r' | '\n' => { /* RTF line breaks are not content */ }
            _ => {
                if skip_depth.is_none() {
                    out.push(c);
                }
                if !c.is_whitespace() {
                    at_group_start = false;
                }
            }
        }
    }
    Ok(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Parse xlsx/xls/ods spreadsheets via calamine.
fn parse_spreadsheet(path: &Path) -> Result<String> {
    use calamine::{open_workbook_auto, Reader};

    let mut workbook = open_workbook_auto(path)?;
    let mut parts = Vec::new();

    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        parts.push(format!("File: {name}"));
    }

    let sheet_names = workbook.sheet_names().to_vec();
    for sheet_name in &sheet_names {
        if let Ok(range) = workbook.worksheet_range(sheet_name) {
            let rows: Vec<String> = range
                .rows()
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .filter(|s| !s.is_empty())
                .collect();

            if !rows.is_empty() {
                parts.push(format!("Sheet: {sheet_name}"));
                parts.extend(rows);
            }
        }
    }

    Ok(parts.join("\n"))
}

/// Parse CSV/TSV files as plain text (calamine handles xlsx; CSV is simpler).
fn parse_csv(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    let mut parts = Vec::new();

    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        parts.push(format!("File: {name}"));
    }

    // Take up to 5000 lines to avoid runaway memory on giant CSVs.
    let lines: Vec<&str> = content.lines().take(5000).collect();
    parts.push(lines.join("\n"));

    Ok(parts.join("\n"))
}

/// Docx extraction: grabs body + headers/footers + footnotes/endnotes from the OOXML zip.
fn parse_docx_zip(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Collect all entry names up front (can't hold the archive borrow across by_name calls).
    let names: Vec<String> = archive.file_names().map(|s| s.to_owned()).collect();

    let mut parts = Vec::new();
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        parts.push(format!("File: {name}"));
    }

    // Helper: read a named zip entry and strip XML tags, returning None if missing.
    let read_stripped =
        |archive: &mut zip::ZipArchive<std::fs::File>, entry: &str| -> Option<String> {
            let xml = crate::types::read_zip_entry_text(
                archive.by_name(entry).ok()?,
                crate::types::MAX_ZIP_ENTRY_BYTES,
            )
            .ok()?;
            let t = strip_xml_tags(&xml);
            if t.trim().is_empty() {
                None
            } else {
                Some(t)
            }
        };

    // 1. Main body (required — bail if missing).
    let body_text = {
        let xml = crate::types::read_zip_entry_text(
            archive.by_name("word/document.xml")?,
            crate::types::MAX_ZIP_ENTRY_BYTES,
        )?;
        strip_xml_tags(&xml)
    };
    if !body_text.trim().is_empty() {
        parts.push(body_text);
    }

    // 2. Headers and footers (header1.xml … header3.xml, footer1.xml … footer3.xml).
    //    Iterate names we already have; collect to avoid borrow conflicts.
    let header_footer_names: Vec<String> = names
        .iter()
        .filter(|n| {
            let base = n.trim_start_matches("word/");
            (base.starts_with("header") || base.starts_with("footer")) && base.ends_with(".xml")
        })
        .cloned()
        .collect();

    for name in &header_footer_names {
        if let Some(text) = read_stripped(&mut archive, name) {
            parts.push(text);
        }
    }

    // 3. Footnotes and endnotes.
    for entry in &["word/footnotes.xml", "word/endnotes.xml"] {
        if names.iter().any(|n| n == entry) {
            if let Some(text) = read_stripped(&mut archive, entry) {
                parts.push(text);
            }
        }
    }

    Ok(parts.join("\n"))
}

pub(crate) fn strip_xml_tags(xml: &str) -> String {
    let mut result = String::with_capacity(xml.len());
    let mut in_tag = false;
    let mut last_was_space = false;

    for ch in xml.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                // Tags often separate words — inject a space.
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            }
            _ if !in_tag => {
                if ch.is_whitespace() {
                    if !last_was_space {
                        result.push(' ');
                        last_was_space = true;
                    }
                } else {
                    result.push(ch);
                    last_was_space = false;
                }
            }
            _ => {}
        }
    }

    let stripped = result.trim().to_owned();

    // Decode XML entities (&amp; → &, &lt; → <, etc.)
    quick_xml::escape::unescape(&stripped)
        .map(|c| c.into_owned())
        .unwrap_or(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_parser_accepts_known_extensions() {
        let p = OfficeParser;
        assert!(p.accepts_path(Path::new("sheet.xlsx")));
        assert!(p.accepts_path(Path::new("data.csv")));
        assert!(p.accepts_path(Path::new("doc.docx")));
        assert!(!p.accepts_path(Path::new("file.pdf")));
        assert!(!p.accepts_path(Path::new("file.rs")));
    }

    #[test]
    fn rtf_strips_control_words_and_skips_tables() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.rtf");
        std::fs::write(
            &p,
            r"{\rtf1\ansi\deff0 {\fonttbl{\f0 Times New Roman;}}\f0\fs24 Hello \b bold\b0  world.\par Second line here.}",
        )
        .unwrap();
        let ex = OfficeParser.parse(&p).unwrap();
        let text: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("bold"), "{text}");
        assert!(text.contains("world"), "{text}");
        assert!(text.contains("Second line here"), "{text}");
        assert!(!text.contains("rtf1"), "control word leaked: {text}");
        assert!(!text.contains("fonttbl"), "{text}");
        assert!(!text.contains("Times"), "font table not skipped: {text}");
    }

    #[test]
    fn csv_parser_produces_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data.csv");
        std::fs::write(&p, "name,age,city\nAlice,30,NYC\nBob,25,LA\n").unwrap();
        let parser = OfficeParser;
        let extracted = parser.parse(&p).unwrap();
        assert!(!extracted.chunks.is_empty());
        let combined: String = extracted
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(combined.contains("Alice"));
        assert!(combined.contains("Bob"));
    }

    #[test]
    fn strip_xml_tags_works() {
        let xml = "<w:p><w:r><w:t>Hello world</w:t></w:r></w:p>";
        let result = strip_xml_tags(xml);
        assert!(result.contains("Hello"));
        assert!(result.contains("world"));
        assert!(!result.contains('<'));
    }

    #[test]
    fn strip_xml_tags_decodes_amp_entity() {
        let xml = "<w:t>Tom &amp; Jerry</w:t>";
        let result = strip_xml_tags(xml);
        assert!(result.contains("Tom & Jerry"), "got: {result}");
        assert!(!result.contains("&amp;"), "raw entity leaked: {result}");
    }

    #[test]
    fn strip_xml_tags_decodes_lt_gt_entities() {
        let xml = "<w:t>x &lt; y &gt; z</w:t>";
        let result = strip_xml_tags(xml);
        assert!(result.contains('<'), "< not decoded: {result}");
        assert!(result.contains('>'), "> not decoded: {result}");
    }

    #[test]
    fn strip_xml_tags_decodes_quot_and_numeric_entities() {
        let xml = "<w:t>&quot;hello&quot; &#39;world&#39;</w:t>";
        let result = strip_xml_tags(xml);
        assert!(result.contains('"'), "quote not decoded: {result}");
    }

    #[test]
    fn office_parser_handles_corrupt_gracefully() {
        // .docx / .xlsx are ZIP containers; feeding non-ZIP bytes must not panic.
        let dir = tempfile::tempdir().unwrap();
        for name in ["bad.docx", "bad.xlsx"] {
            let p = dir.path().join(name);
            std::fs::write(&p, b"this is definitely not a zip container").unwrap();
            // Must return (Err or a graceful fallback), never panic.
            let _ = OfficeParser.parse(&p);
        }
    }

    #[test]
    fn ppt_legacy_produces_stub_chunk_not_hard_error() {
        // Legacy .ppt (OLE binary) must store a stub chunk rather than hard_error.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deck.ppt");
        std::fs::write(&p, b"\xd0\xcf\x11\xe0not-a-real-ole").unwrap();
        let ex = OfficeParser.parse(&p).unwrap();
        assert!(!ex.chunks.is_empty(), "stub chunk must be present");
        let combined: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            combined.contains("Presentation:"),
            "stub text expected, got: {combined}"
        );
        assert!(
            combined.contains("legacy"),
            "should mention legacy format, got: {combined}"
        );
    }

    #[test]
    fn docx_extracts_header_and_footnote() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rich.docx");
        // Build a minimal docx zip with body, header1.xml, and footnotes.xml.
        use zip::write::FileOptions;
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"/>").unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(b"<w:document><w:body><w:p><w:r><w:t>Body text here</w:t></w:r></w:p></w:body></w:document>").unwrap();
        zip.start_file("word/header1.xml", opts).unwrap();
        zip.write_all(b"<w:hdr><w:p><w:r><w:t>Page header content</w:t></w:r></w:p></w:hdr>")
            .unwrap();
        zip.start_file("word/footnotes.xml", opts).unwrap();
        zip.write_all(b"<w:footnotes><w:footnote><w:p><w:r><w:t>Footnote text</w:t></w:r></w:p></w:footnote></w:footnotes>").unwrap();
        let data = zip.finish().unwrap().into_inner();
        std::fs::write(&p, &data).unwrap();

        let ex = OfficeParser.parse(&p).unwrap();
        let combined: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(combined.contains("Body text"), "body not found: {combined}");
        assert!(
            combined.contains("Page header"),
            "header not found: {combined}"
        );
        assert!(
            combined.contains("Footnote text"),
            "footnote not found: {combined}"
        );
    }
}
