//! Office format parser: xlsx/csv via calamine, plain-text extraction for docx.

use crate::types::{Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct OfficeParser;

impl Parser for OfficeParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("xlsx" | "xls" | "xlsm" | "ods" | "csv" | "tsv" | "docx" | "odt" | "rtf")
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
        )
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
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
            _ => "application/octet-stream",
        };

        // Split into ~800-word chunks with 100-word overlap.
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
            let size = 800usize;
            let overlap = 100usize;
            let mut start = 0;
            loop {
                let end = (start + size).min(words.len());
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: String::new(),
                    text: words[start..end].join(" "),
                    language: None,
                });
                seq += 1;
                if end == words.len() {
                    break;
                }
                start += size - overlap;
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: mime.to_owned(),
            chunks,
        })
    }
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

/// Minimal docx extraction: unzip and grab word/document.xml, strip XML tags.
fn parse_docx_zip(path: &Path) -> Result<String> {
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut xml_content = String::new();
    {
        let mut doc = archive.by_name("word/document.xml")?;
        doc.read_to_string(&mut xml_content)?;
    }

    // Strip XML tags — crude but effective for search indexing purposes.
    let text = strip_xml_tags(&xml_content);

    let mut parts = Vec::new();
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        parts.push(format!("File: {name}"));
    }
    if !text.trim().is_empty() {
        parts.push(text);
    }

    Ok(parts.join("\n"))
}

fn strip_xml_tags(xml: &str) -> String {
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
}
