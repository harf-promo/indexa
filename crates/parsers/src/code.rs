use crate::types::{Chunk, Extracted, Parser};
use anyhow::Result;
use std::path::Path;
use tree_sitter::Node;

/// Maximum characters per code chunk.
const MAX_CHUNK_CHARS: usize = 2000;
/// Minimum characters — skip tiny nodes like single-line stubs.
const MIN_CHUNK_CHARS: usize = 20;

pub struct CodeParser;

impl CodeParser {
    /// Extension-based detection (preferred).
    /// mime_guess misidentifies: .py → text/plain, .ts → video, .go/.java → octet-stream.
    fn language_for_path(path: &Path) -> Option<LanguageDef> {
        let ext = path.extension()?.to_str()?;
        match ext {
            "rs" => Some(LanguageDef {
                ts_lang: tree_sitter_rust::LANGUAGE.into(),
                name: "rust",
                top_level_kinds: &[
                    "function_item",
                    "impl_item",
                    "struct_item",
                    "enum_item",
                    "trait_item",
                    "mod_item",
                    "type_alias",
                    "const_item",
                    "static_item",
                ],
            }),
            "py" => Some(LanguageDef {
                ts_lang: tree_sitter_python::LANGUAGE.into(),
                name: "python",
                top_level_kinds: &[
                    "function_definition",
                    "class_definition",
                    "decorated_definition",
                ],
            }),
            "js" | "mjs" | "cjs" => Some(LanguageDef {
                ts_lang: tree_sitter_javascript::LANGUAGE.into(),
                name: "javascript",
                top_level_kinds: &[
                    "function_declaration",
                    "class_declaration",
                    "export_statement",
                    "lexical_declaration",
                    "variable_declaration",
                ],
            }),
            "ts" | "mts" | "cts" => Some(LanguageDef {
                ts_lang: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                name: "typescript",
                top_level_kinds: &[
                    "function_declaration",
                    "class_declaration",
                    "interface_declaration",
                    "type_alias_declaration",
                    "export_statement",
                    "lexical_declaration",
                ],
            }),
            "tsx" => Some(LanguageDef {
                ts_lang: tree_sitter_typescript::LANGUAGE_TSX.into(),
                name: "tsx",
                top_level_kinds: &[
                    "function_declaration",
                    "class_declaration",
                    "interface_declaration",
                    "export_statement",
                    "lexical_declaration",
                ],
            }),
            "go" => Some(LanguageDef {
                ts_lang: tree_sitter_go::LANGUAGE.into(),
                name: "go",
                top_level_kinds: &[
                    "function_declaration",
                    "method_declaration",
                    "type_declaration",
                    "const_declaration",
                    "var_declaration",
                ],
            }),
            "java" => Some(LanguageDef {
                ts_lang: tree_sitter_java::LANGUAGE.into(),
                name: "java",
                top_level_kinds: &[
                    "class_declaration",
                    "interface_declaration",
                    "method_declaration",
                    "enum_declaration",
                ],
            }),
            _ => None,
        }
    }

    /// MIME-type based fallback detection.
    fn language_for_mime(mime: &str) -> Option<LanguageDef> {
        match mime {
            "text/x-rust" | "text/rust" => Some(LanguageDef {
                ts_lang: tree_sitter_rust::LANGUAGE.into(),
                name: "rust",
                top_level_kinds: &[
                    "function_item",
                    "impl_item",
                    "struct_item",
                    "enum_item",
                    "trait_item",
                ],
            }),
            "text/x-python" | "text/x-python3" => Some(LanguageDef {
                ts_lang: tree_sitter_python::LANGUAGE.into(),
                name: "python",
                top_level_kinds: &["function_definition", "class_definition"],
            }),
            "application/javascript" | "text/javascript" | "text/x-javascript" => {
                Some(LanguageDef {
                    ts_lang: tree_sitter_javascript::LANGUAGE.into(),
                    name: "javascript",
                    top_level_kinds: &[
                        "function_declaration",
                        "class_declaration",
                        "export_statement",
                    ],
                })
            }
            "text/x-go" | "text/go" => Some(LanguageDef {
                ts_lang: tree_sitter_go::LANGUAGE.into(),
                name: "go",
                top_level_kinds: &["function_declaration", "method_declaration"],
            }),
            "text/x-java" | "text/java" => Some(LanguageDef {
                ts_lang: tree_sitter_java::LANGUAGE.into(),
                name: "java",
                top_level_kinds: &["class_declaration", "interface_declaration"],
            }),
            _ => None,
        }
    }
}

struct LanguageDef {
    ts_lang: tree_sitter::Language,
    name: &'static str,
    top_level_kinds: &'static [&'static str],
}

impl Parser for CodeParser {
    fn accepts_path(&self, path: &Path) -> bool {
        Self::language_for_path(path).is_some()
    }

    fn accepts_mime(&self, mime: &str) -> bool {
        Self::language_for_mime(mime).is_some()
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        let source = std::fs::read_to_string(path)?;
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        // Extension-based detection preferred; MIME-based as fallback.
        let lang_def = Self::language_for_path(path)
            .or_else(|| Self::language_for_mime(&mime))
            .ok_or_else(|| anyhow::anyhow!("no language detected for {}", path.display()))?;

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang_def.ts_lang)?;
        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))?;

        let root = tree.root_node();
        let mut chunks = Vec::new();
        let mut seq = 0usize;

        extract_chunks(
            root,
            &source,
            path,
            lang_def.name,
            lang_def.top_level_kinds,
            &mut chunks,
            &mut seq,
        );

        // If tree-sitter found nothing (e.g. script-style file), fall back to fixed windows.
        if chunks.is_empty() {
            let words: Vec<&str> = source.split_whitespace().collect();
            let mut start = 0;
            while start < words.len() {
                let end = (start + 400).min(words.len());
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq,
                    heading: String::new(),
                    text: words[start..end].join(" "),
                    language: Some(lang_def.name.to_owned()),
                });
                seq += 1;
                if end == words.len() {
                    break;
                }
                start += 300;
            }
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime,
            chunks,
        })
    }
}

fn extract_chunks(
    node: Node,
    source: &str,
    path: &Path,
    language: &str,
    kinds: &[&str],
    chunks: &mut Vec<Chunk>,
    seq: &mut usize,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kinds.contains(&kind) {
            let text = &source[child.byte_range()];
            if text.len() < MIN_CHUNK_CHARS {
                continue;
            }
            if text.len() > MAX_CHUNK_CHARS {
                let words: Vec<&str> = text.split_whitespace().collect();
                let mut start = 0;
                while start < words.len() {
                    let end = (start + 400).min(words.len());
                    chunks.push(Chunk {
                        source: path.to_path_buf(),
                        seq: *seq,
                        heading: symbol_name(&child, source),
                        text: words[start..end].join(" "),
                        language: Some(language.to_owned()),
                    });
                    *seq += 1;
                    if end == words.len() {
                        break;
                    }
                    start += 300;
                }
            } else {
                chunks.push(Chunk {
                    source: path.to_path_buf(),
                    seq: *seq,
                    heading: symbol_name(&child, source),
                    text: text.to_owned(),
                    language: Some(language.to_owned()),
                });
                *seq += 1;
            }
        } else {
            extract_chunks(child, source, path, language, kinds, chunks, seq);
        }
    }
}

/// Try to extract a meaningful name from a top-level AST node.
fn symbol_name(node: &Node, source: &str) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "name" || k == "type_identifier" {
            return source[child.byte_range()].to_owned();
        }
    }
    node.kind().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.rs");
        std::fs::write(
            &p,
            r#"
pub fn hello(name: &str) -> String {
    format!("Hello, {name}!")
}

pub struct Greeter { prefix: String }

impl Greeter {
    pub fn greet(&self, name: &str) -> String {
        format!("{} {name}", self.prefix)
    }
}
"#,
        )
        .unwrap();

        let parser = CodeParser;
        let extracted = parser.parse(&p).unwrap();
        assert!(extracted.chunks.len() >= 2);
        assert_eq!(extracted.chunks[0].language.as_deref(), Some("rust"));
    }

    #[test]
    fn parses_python_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main.py");
        std::fs::write(
            &p,
            "def greet(name: str) -> str:\n    return f'Hello, {name}!'\n\nclass Greeter:\n    pass\n",
        )
        .unwrap();

        let parser = CodeParser;
        let extracted = parser.parse(&p).unwrap();
        assert!(!extracted.chunks.is_empty());
        assert_eq!(extracted.chunks[0].language.as_deref(), Some("python"));
    }

    #[test]
    fn code_parser_accepts_by_extension() {
        let parser = CodeParser;
        assert!(parser.accepts_path(Path::new("file.rs")));
        assert!(parser.accepts_path(Path::new("file.py")));
        assert!(parser.accepts_path(Path::new("file.ts")));
        assert!(parser.accepts_path(Path::new("file.go")));
        assert!(!parser.accepts_path(Path::new("file.txt")));
        assert!(!parser.accepts_path(Path::new("file.md")));
    }

    #[test]
    fn code_parser_handles_broken_source_gracefully() {
        // Syntactically broken source makes tree-sitter emit ERROR nodes; it must
        // not panic, and should still return without aborting (chunks or empty).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("broken.rs");
        std::fs::write(&p, "fn ( { unclosed <<<>>> ??? impl for 123").unwrap();
        let extracted = CodeParser
            .parse(&p)
            .expect("must not panic on broken source");
        // Behaviour is best-effort; we only require it returned cleanly.
        let _ = extracted.chunks;
    }
}
