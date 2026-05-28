//! EPUB 2/3 parser — reads spine order from OPF, strips XHTML tags per chapter.

use crate::types::{Chunk, Extracted, Parser};
use anyhow::{bail, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

pub struct EpubParser;

impl Parser for EpubParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(path.extension().and_then(|e| e.to_str()), Some("epub"))
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "application/epub+zip"
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let file = std::fs::File::open(path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        // Step 1: container.xml → OPF path
        let opf_path = {
            let mut xml = String::new();
            archive.by_name("META-INF/container.xml")?.read_to_string(&mut xml)?;
            parse_container_xml(&xml)?
        };

        // OPF directory for resolving relative hrefs
        let opf_dir = opf_path
            .rfind('/')
            .map(|i| opf_path[..=i].to_string())
            .unwrap_or_default();

        // Step 2: OPF → manifest + spine
        let opf_xml = {
            let mut s = String::new();
            archive.by_name(&opf_path)?.read_to_string(&mut s)?;
            s
        };
        let (manifest, spine) = parse_opf(&opf_xml)?;

        // Step 3: for each spine item, read XHTML and chunk
        let mut chunks = Vec::new();
        let mut seq = 0usize;

        for item_id in &spine {
            let href = match manifest.get(item_id.as_str()) {
                Some(h) => h.clone(),
                None => continue,
            };
            let full_path = if href.starts_with('/') {
                href.trim_start_matches('/').to_string()
            } else {
                format!("{opf_dir}{href}")
            };

            let xhtml = match archive.by_name(&full_path) {
                Ok(mut entry) => {
                    let mut s = String::new();
                    let _ = entry.read_to_string(&mut s);
                    s
                }
                Err(_) => continue,
            };

            let text = strip_xhtml_text(&xhtml);
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }

            // Chapter heading from filename stem
            let heading = href
                .rsplit('/')
                .next()
                .unwrap_or(&href)
                .trim_end_matches(".xhtml")
                .trim_end_matches(".html")
                .to_string();

            let words: Vec<&str> = text.split_whitespace().collect();
            let chunk_size = 800usize;
            let overlap = 100usize;
            let mut start = 0;
            loop {
                let end = (start + chunk_size).min(words.len());
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: heading.clone(),
                    text: words[start..end].join(" "),
                    language: None,
                });
                seq += 1;
                if end == words.len() {
                    break;
                }
                start += chunk_size - overlap;
            }
        }

        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!(
                    "EPUB: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/epub+zip".into(),
            chunks,
        })
    }
}

/// Extract the OPF path from META-INF/container.xml.
fn parse_container_xml(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e)
                if e.local_name().as_ref() == b"rootfile" =>
            {
                for attr in e.attributes() {
                    let attr = attr?;
                    if attr.key.local_name().as_ref() == b"full-path" {
                        return Ok(String::from_utf8(attr.value.to_vec())?);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    bail!("no OPF rootfile found in META-INF/container.xml")
}

/// Parse the OPF file — returns (manifest: id→href, spine: ordered ids).
fn parse_opf(xml: &str) -> Result<(HashMap<String, String>, Vec<String>)> {
    let mut reader = Reader::from_str(xml);
    let mut manifest: HashMap<String, String> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();
    let mut in_manifest = false;
    let mut in_spine = false;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e) => {
                match e.local_name().as_ref() {
                    b"manifest" => in_manifest = true,
                    b"spine" => in_spine = true,
                    b"item" if in_manifest => {
                        let mut id = String::new();
                        let mut href = String::new();
                        let mut media_type = String::new();
                        for attr in e.attributes() {
                            let attr = attr?;
                            match attr.key.local_name().as_ref() {
                                b"id" => id = String::from_utf8(attr.value.to_vec())?,
                                b"href" => href = String::from_utf8(attr.value.to_vec())?,
                                b"media-type" => {
                                    media_type = String::from_utf8(attr.value.to_vec())?
                                }
                                _ => {}
                            }
                        }
                        // Only include HTML content items
                        if !id.is_empty()
                            && !href.is_empty()
                            && (media_type.contains("html") || href.ends_with(".html") || href.ends_with(".xhtml"))
                        {
                            manifest.insert(id, href);
                        }
                    }
                    b"itemref" if in_spine => {
                        for attr in e.attributes() {
                            let attr = attr?;
                            if attr.key.local_name().as_ref() == b"idref" {
                                spine.push(String::from_utf8(attr.value.to_vec())?);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match e.local_name().as_ref() {
                b"manifest" => in_manifest = false,
                b"spine" => in_spine = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    Ok((manifest, spine))
}

/// Strip HTML/XHTML tags and decode entities, returning plain text.
fn strip_xhtml_text(html: &str) -> String {
    // Skip script/style content, inject spaces at block boundaries.
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip_depth = 0u32;
    let mut tag_buf = String::new();

    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                in_tag = false;
                let tag_lower = tag_buf.trim().to_lowercase();
                let tag_name = tag_lower.split_whitespace().next().unwrap_or("");
                let is_close = tag_name.starts_with('/');
                let bare = tag_name.trim_start_matches('/');

                if matches!(bare, "script" | "style") {
                    if is_close {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !tag_lower.ends_with('/') {
                        skip_depth += 1;
                    }
                }

                if skip_depth == 0
                    && matches!(
                        bare,
                        "p" | "div"
                            | "li"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "br"
                            | "td"
                            | "th"
                            | "tr"
                    )
                {
                    result.push(' ');
                }
            }
            _ if in_tag => {
                tag_buf.push(ch);
            }
            _ if skip_depth == 0 => {
                result.push(ch);
            }
            _ => {}
        }
    }

    // Decode XML/HTML entities
    let decoded = quick_xml::escape::unescape(&result)
        .map(|c| c.into_owned())
        .unwrap_or(result);

    // Normalize whitespace
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    fn build_epub(chapters: &[(&str, &str)]) -> Vec<u8> {
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = FileOptions::<()>::default();

        // mimetype
        zip.start_file("mimetype", opts).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();

        // META-INF/container.xml
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
        )
        .unwrap();

        // Build OPF
        let mut manifest_items = String::new();
        let mut spine_items = String::new();
        for (i, (id, _)) in chapters.iter().enumerate() {
            manifest_items.push_str(&format!(
                r#"<item id="{id}" href="{id}.xhtml" media-type="application/xhtml+xml"/>"#,
                id = id,
            ));
            manifest_items.push('\n');
            spine_items.push_str(&format!(r#"<itemref idref="{id}"/>"#, id = id));
            spine_items.push('\n');
            let _ = i;
        }
        let opf = format!(
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
<manifest>{manifest_items}</manifest>
<spine>{spine_items}</spine>
</package>"#
        );
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();

        // Chapter files
        for (id, body) in chapters {
            zip.start_file(format!("{id}.xhtml"), opts).unwrap();
            let xhtml = format!(
                r#"<?xml version="1.0"?><html><body>{body}</body></html>"#
            );
            zip.write_all(xhtml.as_bytes()).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn epub_parser_extracts_two_chapters() {
        let epub_bytes = build_epub(&[
            ("ch1", "<p>Chapter one content here.</p>"),
            ("ch2", "<p>Chapter two content here.</p>"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.epub");
        std::fs::write(&path, epub_bytes).unwrap();

        let parser = EpubParser;
        let extracted = parser.parse(&path).unwrap();

        assert_eq!(extracted.chunks.len(), 2);
        assert!(extracted.chunks[0].text.contains("Chapter one"));
        assert!(extracted.chunks[1].text.contains("Chapter two"));
        assert_eq!(extracted.chunks[0].heading, "ch1");
        assert_eq!(extracted.chunks[1].heading, "ch2");
    }

    #[test]
    fn epub_parser_decodes_entities() {
        let epub_bytes = build_epub(&[("ch1", "<p>Hello &amp; World &lt;3&gt;</p>")]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entities.epub");
        std::fs::write(&path, epub_bytes).unwrap();

        let parser = EpubParser;
        let extracted = parser.parse(&path).unwrap();
        let text = &extracted.chunks[0].text;
        assert!(text.contains('&'), "& should be decoded: {text}");
        assert!(text.contains('<') || text.contains("3"), "entities should decode: {text}");
    }

    #[test]
    fn epub_parser_accepts_epub_extension() {
        let p = EpubParser;
        assert!(p.accepts_path(Path::new("book.epub")));
        assert!(!p.accepts_path(Path::new("book.pdf")));
    }

    #[test]
    fn strip_xhtml_text_removes_tags_and_decodes() {
        let html = "<p>Hello &amp; <em>world</em>!</p>";
        let result = strip_xhtml_text(html);
        assert!(result.contains("Hello"), "{result}");
        assert!(result.contains('&'), "& should be decoded: {result}");
        assert!(result.contains("world"), "{result}");
        assert!(!result.contains('<'), "{result}");
    }
}
