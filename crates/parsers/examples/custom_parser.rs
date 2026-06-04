//! Plugin SDK example — demonstrates how to implement a custom parser and
//! register it with the Indexa parser registry.
//!
//! This example adds a minimal plain-text parser for `.mydata` files.
//! Run with:
//!   cargo run --example custom_parser --manifest-path crates/parsers/Cargo.toml
//!
//! A real plugin would:
//! 1. Depend on `indexa-parsers` from crates.io or a git path.
//! 2. Implement `Parser` for its file format.
//! 3. Call `Registry::new(); reg.register(Box::new(MyParser))` in a custom main
//!    binary (or expose it via a Rust API if building an extension library).

use indexa_parsers::{
    registry::Registry,
    types::{Chunk, Extracted, Parser},
};
use std::path::Path;

// ── Example plugin parser ─────────────────────────────────────────────────────

/// A trivial parser for `.mydata` files (reads them as plain text).
/// Replace this with real format-specific parsing for your file type.
struct MyDataParser;

impl Parser for MyDataParser {
    fn accepts_path(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("mydata")
    }

    fn accepts_mime(&self, _mime: &str) -> bool {
        false // rely on accepts_path instead
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Extracted> {
        let text = std::fs::read_to_string(path)?;
        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/x-mydata".to_owned(),
            chunks: vec![Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text,
                language: None,
            }],
            edges: Vec::new(),
        })
    }
}

// ── Demo ──────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    // Build a registry with the built-in parsers + our custom one.
    let mut registry = Registry::new();
    registry.register(Box::new(MyDataParser));

    // Write a temp file with a `.mydata` extension and parse it.
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("sample.mydata");
    std::fs::write(&file, "hello from my custom parser")?;

    let extracted = registry.parse(&file)?;
    println!(
        "Parsed {} → {} chunk(s)",
        file.display(),
        extracted.chunks.len()
    );
    for chunk in &extracted.chunks {
        println!("  chunk[{}]: {:?}", chunk.seq, chunk.text);
    }

    // A .txt file still uses the built-in TextParser.
    let txt = dir.path().join("note.txt");
    std::fs::write(&txt, "hello from built-in text parser")?;
    let ex2 = registry.parse(&txt)?;
    println!(
        "Parsed {} → {} chunk(s) via built-in",
        txt.display(),
        ex2.chunks.len()
    );

    Ok(())
}
