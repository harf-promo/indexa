//! Binary symbol extraction: list the symbols a compiled artifact declares, so "what defines
//! `mint_token`" can reach a `.so`/`.dylib`/`.exe`/`.o`, a `.wasm` is searchable by its exports,
//! and a `.jar` by its class names. **Names only** — no disassembly. Pure-Rust (`object`,
//! `wasmparser`); a stripped or unreadable binary yields a quiet stub.

use crate::types::{chunk_words, Chunk, ChunkParams, Extracted, Parser};
use anyhow::Result;
use std::path::Path;

pub struct BinaryParser;

/// Cap the symbol list so a huge binary can't blow up a chunk/memory.
const MAX_SYMBOLS: usize = 4000;

impl Parser for BinaryParser {
    fn accepts_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("so" | "dylib" | "exe" | "wasm" | "jar" | "o")
        )
    }

    /// Dispatched by extension only — never by MIME (these sniff as octet-stream).
    fn accepts_mime(&self, _mime: &str) -> bool {
        false
    }

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[
            ("so", Metadata),
            ("dylib", Metadata),
            ("exe", Metadata),
            ("o", Metadata),
            ("wasm", Metadata),
            ("jar", Metadata),
        ]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let symbols = match ext {
            "wasm" => wasm_exports(path),
            "jar" => jar_classes(path),
            _ => object_symbols(path),
        }
        .unwrap_or_default();

        let text = if symbols.is_empty() {
            format!("Binary: {display} (no readable symbols — may be stripped)")
        } else {
            format!(
                "Binary {display} — {} symbols:\n{}",
                symbols.len(),
                symbols.join("\n")
            )
        };

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        chunk_words(
            path,
            &text,
            "symbols",
            None,
            chunk.size,
            chunk.overlap,
            &mut seq,
            &mut chunks,
        );
        if chunks.is_empty() {
            chunks.push(Chunk {
                source: path.to_path_buf(),
                seq: 0,
                heading: String::new(),
                text: format!("Binary: {display}"),
                language: None,
            });
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/octet-stream".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// ELF / Mach-O / PE / COFF symbol names via the pure-Rust `object` crate.
fn object_symbols(path: &Path) -> Result<Vec<String>> {
    use object::{Object, ObjectSymbol};
    let data = std::fs::read(path)?;
    let file = object::File::parse(&*data)?;
    let mut names: Vec<String> = Vec::new();
    for sym in file.symbols().chain(file.dynamic_symbols()) {
        if names.len() >= MAX_SYMBOLS {
            break;
        }
        if let Ok(name) = sym.name() {
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// WebAssembly export names via `wasmparser`.
fn wasm_exports(path: &Path) -> Result<Vec<String>> {
    let data = std::fs::read(path)?;
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&data) {
        if let Ok(wasmparser::Payload::ExportSection(exports)) = payload {
            for export in exports.into_iter().flatten() {
                if names.len() >= MAX_SYMBOLS {
                    break;
                }
                names.push(export.name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Fully-qualified class names from a `.jar` (it's a zip of `.class` files).
fn jar_classes(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut names = Vec::new();
    for i in 0..zip.len().min(MAX_SYMBOLS) {
        if let Ok(f) = zip.by_index(i) {
            let n = f.name();
            if n.ends_with(".class") {
                names.push(n.trim_end_matches(".class").replace('/', "."));
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_accepts_extensions() {
        let p = BinaryParser;
        assert!(p.accepts_path(Path::new("/x/libfoo.so")));
        assert!(p.accepts_path(Path::new("/x/app.wasm")));
        assert!(p.accepts_path(Path::new("/x/lib.jar")));
        assert!(!p.accepts_path(Path::new("/x/main.rs")));
    }

    #[test]
    fn wasm_exports_are_listed() {
        // Minimal valid wasm module exporting a function "run".
        // (module (func (export "run")))  → hand-assembled binary.
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: () -> ()
            0x03, 0x02, 0x01, 0x00, // func section: 1 func, type 0
            0x07, 0x07, 0x01, 0x03, b'r', b'u', b'n', 0x00, 0x00, // export "run" func 0
            0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // code section: empty body
        ];
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.wasm");
        std::fs::write(&p, wasm).unwrap();
        let ex = BinaryParser.parse(&p).unwrap();
        let all: String = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("run"), "{all}");
    }

    #[test]
    fn unreadable_binary_is_a_stub() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("junk.so");
        std::fs::write(&p, b"not a real ELF").unwrap();
        let ex = BinaryParser.parse(&p).unwrap();
        assert!(ex.chunks[0].text.contains("Binary"));
    }
}
