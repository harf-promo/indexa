//! Persistent notes — the "write-back" half of the acontext learning loop.
//!
//! An AI caller that learns something new can persist it as a Markdown note in
//! the Indexa data directory. Notes are plain files that flow through the normal
//! indexing pipeline, so they become immediately searchable after a
//! `trigger_index` call.
//!
//! The design mirrors the "cache-as-file" pattern used by `indexa pack add-url`:
//! write a local file → `add_pack_paths` → index. No new schema; notes inherit
//! secret redaction on export automatically (they live inside a pack).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Lowercase, keep alphanumerics, collapse other characters to single dashes,
/// cap at 48 chars. A degenerate input that collapses entirely returns `"note"`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(48));
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        "note".to_owned()
    } else {
        trimmed
    }
}

/// Write a note to `<data_dir>/notes/<slug>-<body_sha8>.md`.
///
/// The filename is keyed on both the title slug and a hash of the body, so
/// re-submitting the same note with the same content is idempotent (it
/// overwrites the existing file). Changing the body produces a new file (new
/// content deserves a fresh index entry).
///
/// Returns the path of the written file so the caller can pass it to
/// `store.add_pack_paths` and then trigger indexing.
pub fn write_note_file(
    data_dir: &Path,
    pack: &str,
    title: &str,
    body: &str,
) -> anyhow::Result<PathBuf> {
    let dir = data_dir.join("notes");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating notes dir {}: {e}", dir.display()))?;

    let slug = slugify(title);
    let sha = format!("{:x}", Sha256::digest(body.as_bytes()));
    let filename = format!("{slug}-{}.md", &sha[..8]);
    let path = dir.join(&filename);

    // Provenance header + title heading + body. The header is a Markdown comment so
    // it is ignored by renderers but visible in raw text, matching the remote-source
    // pattern (which uses `<!-- indexa remote source: … -->`).
    let content =
        format!("<!-- indexa note: pack={pack} title={title:?} -->\n\n# {title}\n\n{body}\n");
    std::fs::write(&path, &content)
        .map_err(|e| anyhow::anyhow!("writing note {}: {e}", path.display()))?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Rust & Cargo"), "rust-cargo");
        assert_eq!(slugify("!!!"), "note");
        assert_eq!(slugify("already-fine-123"), "already-fine-123");
    }

    #[test]
    fn slugify_caps_at_48() {
        let long = "a".repeat(100);
        assert!(slugify(&long).len() <= 48);
    }

    #[test]
    fn write_note_file_creates_file_with_expected_content() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_note_file(dir.path(), "my-pack", "Test Note", "Body content here.").unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# Test Note"));
        assert!(contents.contains("Body content here."));
        assert!(contents.contains("pack=my-pack"));
    }

    #[test]
    fn same_content_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = write_note_file(dir.path(), "pack", "Title", "body").unwrap();
        let p2 = write_note_file(dir.path(), "pack", "Title", "body").unwrap();
        // Same title + body → same filename → idempotent overwrite.
        assert_eq!(p1, p2);
    }

    #[test]
    fn different_body_gives_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = write_note_file(dir.path(), "pack", "Title", "body v1").unwrap();
        let p2 = write_note_file(dir.path(), "pack", "Title", "body v2").unwrap();
        assert_ne!(p1, p2);
    }
}
