//! Email parser (`.eml` / `.msg`): index an email by its headers, body, and attachment names.
//!
//! `.eml` (RFC 822 / MIME) is parsed with `mail-parser` (pure Rust). The plain-text body is
//! preferred; an HTML-only body is converted to text. Attachment *names* are listed — their
//! bytes are not extracted. `.msg` (Outlook OLE compound format) has no pure-Rust reader, so
//! it gets a quiet stub rather than counting as a hard parse error.

use crate::types::{chunk_words, Chunk, Extracted, Parser};
use anyhow::Result;
use mail_parser::{Address, Message, MessageParser, MimeHeaders};
use std::path::Path;

pub struct EmailParser;

impl Parser for EmailParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("eml" | "msg")
        )
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        mime == "message/rfc822"
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[("eml", Full), ("msg", Stub)]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let text = if ext == "msg" {
            format!("Email: {display} (Outlook .msg — headers/body not extracted)")
        } else {
            let bytes = std::fs::read(path)?;
            match MessageParser::default().parse(&bytes) {
                Some(msg) => render_email(&msg),
                None => format!("Email: {display} (unparseable)"),
            }
        };

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        chunk_words(path, &text, "email", None, 800, 100, &mut seq, &mut chunks);
        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!("Email: {display}"),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "message/rfc822".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Render an email's headers + body + attachment names into one searchable text block.
fn render_email(msg: &Message) -> String {
    let mut out = String::new();
    if let Some(s) = msg.subject() {
        out.push_str(&format!("Subject: {s}\n"));
    }
    if let Some(from) = msg.from() {
        out.push_str(&format!("From: {}\n", addr_str(from)));
    }
    if let Some(to) = msg.to() {
        out.push_str(&format!("To: {}\n", addr_str(to)));
    }
    if let Some(date) = msg.date() {
        out.push_str(&format!("Date: {date}\n"));
    }

    // Body: prefer the plain-text part; fall back to the HTML part converted to Markdown.
    if let Some(body) = msg.body_text(0) {
        out.push('\n');
        out.push_str(body.trim());
    } else if let Some(html) = msg.body_html(0) {
        out.push('\n');
        out.push_str(
            htmd::convert(&html)
                .unwrap_or_else(|_| html.into_owned())
                .trim(),
        );
    }

    // Attachment names (not contents).
    let names: Vec<&str> = msg
        .attachments()
        .filter_map(|a| a.attachment_name())
        .collect();
    if !names.is_empty() {
        out.push_str(&format!("\n\nAttachments: {}", names.join(", ")));
    }

    out
}

/// First address of a From/To field as `"Name <addr>"` (or just one of them).
fn addr_str(addr: &Address) -> String {
    addr.first()
        .map(|a| match (a.name(), a.address()) {
            (Some(n), Some(e)) => format!("{n} <{e}>"),
            (None, Some(e)) => e.to_string(),
            (Some(n), None) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eml_extracts_headers_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("msg.eml");
        std::fs::write(
            &p,
            "From: Alice <alice@example.com>\r\n\
             To: Bob <bob@example.com>\r\n\
             Subject: Quarterly report\r\n\
             Date: Mon, 16 Jun 2026 10:00:00 +0000\r\n\
             \r\n\
             The Q2 numbers are ready. The auth migration is on track.\r\n",
        )
        .unwrap();
        let ex = EmailParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("Quarterly report"), "{all}");
        assert!(all.contains("alice@example.com"), "{all}");
        assert!(all.contains("bob@example.com"), "{all}");
        assert!(all.contains("Q2 numbers"), "{all}");
        assert!(all.contains("auth migration"), "{all}");
    }

    #[test]
    fn msg_is_a_quiet_stub() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("outlook.msg");
        std::fs::write(&p, b"\xd0\xcf\x11\xe0 not a real OLE file").unwrap();
        let ex = EmailParser.parse(&p).unwrap();
        assert!(
            ex.chunks[0].text.contains(".msg"),
            "{:?}",
            ex.chunks[0].text
        );
    }

    #[test]
    fn email_accepts_extensions_and_mime() {
        let p = EmailParser;
        assert!(p.accepts_path(Path::new("/x/note.eml")));
        assert!(p.accepts_path(Path::new("/x/note.msg")));
        assert!(!p.accepts_path(Path::new("/x/note.txt")));
        assert!(p.accepts_mime("message/rfc822"));
    }
}
