use crate::text::decode_text_bytes;
use crate::types::{Chunk, Edge, Extracted, Parser};
use anyhow::Result;
use std::path::Path;
use tree_sitter::Node;

/// Maximum bytes per code chunk (compared against `&str::len()`, which is byte length, not chars).
const MAX_CHUNK_BYTES: usize = 2000;
/// Minimum bytes — skip tiny nodes like single-line stubs.
const MIN_CHUNK_BYTES: usize = 20;

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
            // `.h` defaults to C (pragmatic) — C++ headers still parse acceptably with
            // the C grammar for chunking/defines, and the call/include graph survives.
            "c" | "h" => Some(LanguageDef {
                ts_lang: tree_sitter_c::LANGUAGE.into(),
                name: "c",
                top_level_kinds: &[
                    "function_definition",
                    "declaration",
                    "struct_specifier",
                    "enum_specifier",
                    "union_specifier",
                    "type_definition",
                    "preproc_def",
                    "preproc_function_def",
                ],
            }),
            "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" => Some(LanguageDef {
                ts_lang: tree_sitter_cpp::LANGUAGE.into(),
                name: "cpp",
                top_level_kinds: &[
                    "function_definition",
                    "declaration",
                    "class_specifier",
                    "struct_specifier",
                    "enum_specifier",
                    "union_specifier",
                    "namespace_definition",
                    "template_declaration",
                    "type_definition",
                    "preproc_def",
                    "preproc_function_def",
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
            "text/x-c" | "text/x-csrc" => Some(LanguageDef {
                ts_lang: tree_sitter_c::LANGUAGE.into(),
                name: "c",
                top_level_kinds: &["function_definition", "struct_specifier", "enum_specifier"],
            }),
            "text/x-c++" | "text/x-c++src" | "text/x-cpp" => Some(LanguageDef {
                ts_lang: tree_sitter_cpp::LANGUAGE.into(),
                name: "cpp",
                top_level_kinds: &[
                    "function_definition",
                    "class_specifier",
                    "struct_specifier",
                    "namespace_definition",
                ],
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

    fn declared_formats(&self) -> &'static [(&'static str, crate::types::Support)] {
        use crate::types::Support::*;
        &[
            ("rs", Full),
            ("py", Full),
            ("js", Full),
            ("jsx", Full),
            ("mjs", Full),
            ("cjs", Full),
            ("ts", Full),
            ("tsx", Full),
            ("mts", Full),
            ("cts", Full),
            ("go", Full),
            ("java", Full),
            ("c", Full),
            ("h", Full),
            ("cpp", Full),
            ("cc", Full),
            ("cxx", Full),
            ("c++", Full),
            ("hpp", Full),
            ("hh", Full),
            ("hxx", Full),
            ("h++", Full),
        ]
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        // `decode_text_bytes` (always lossy), not a strict `read_to_string`: `CodeParser`
        // doesn't override `parse_chunked`, so it never sees the `[parsers] encoding` config —
        // but that config's own default is `Auto` (lossy), so this matches the default behavior
        // every other text-shaped parser already gives out of the box, instead of hard-erroring
        // a source file that has one stray non-UTF-8 byte in a comment/string literal.
        let source = decode_text_bytes(&std::fs::read(path)?);
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

        // Code-graph edges (D1): `defines` from every named top-level (and nested) symbol
        // node, `imports` from the language's import/use nodes — both walked on the same
        // tree. Defines come from the tree, not from `chunks`, so symbols too small to be
        // chunked (e.g. an empty-bodied `fn`) are still captured.
        let mut edges = Vec::new();
        let mut seen_defs = std::collections::HashSet::new();
        extract_defines(
            root,
            &source,
            path,
            lang_def.top_level_kinds,
            &mut edges,
            &mut seen_defs,
        );
        let import_kinds = import_kinds_for(lang_def.name);
        if !import_kinds.is_empty() {
            extract_imports(root, &source, path, import_kinds, &mut edges);
        }
        // D2: extract call edges (function/method names invoked by this file).
        let call_kinds = call_kinds_for(lang_def.name);
        if !call_kinds.is_empty() {
            let mut seen_calls = std::collections::HashSet::new();
            extract_calls(root, &source, path, call_kinds, &mut edges, &mut seen_calls);
        }
        // 2.2: extract extends/implements heritage edges (class/impl/interface bases).
        let heritage_kinds = heritage_kinds_for(lang_def.name);
        if !heritage_kinds.is_empty() {
            extract_heritage(
                root,
                &source,
                path,
                lang_def.name,
                heritage_kinds,
                &mut edges,
            );
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime,
            chunks,
            edges,
        })
    }
}

/// Tree-sitter node kinds that represent a call expression, by language name.
fn call_kinds_for(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &["call_expression", "method_call_expression"],
        "python" => &["call"],
        "javascript" | "typescript" | "tsx" => &["call_expression"],
        "go" => &["call_expression"],
        "java" => &["method_invocation"],
        "c" | "cpp" => &["call_expression"],
        _ => &[],
    }
}

/// Walk the tree and push one `calls` edge per distinct function/method name invoked.
/// Deduped by name across the whole file so repeated calls produce a single edge.
/// Names shorter than 2 characters are skipped (noise: `f`, `g`, operators, etc.).
fn extract_calls(
    node: Node,
    source: &str,
    path: &Path,
    call_kinds: &[&str],
    edges: &mut Vec<Edge>,
    seen: &mut std::collections::HashSet<String>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if call_kinds.contains(&child.kind()) {
            if let Some(name) = call_callee(&child, source) {
                if name.len() >= 2 && seen.insert(name.clone()) {
                    edges.push(Edge {
                        from: path.to_path_buf(),
                        kind: "calls",
                        to: name,
                        symbol_kind: None,
                        line_range: None,
                    });
                }
            }
        }
        extract_calls(child, source, path, call_kinds, edges, seen);
    }
}

/// Extract the bare callee name from a call node.
///
/// Strategy (applies across all supported languages):
/// 1. Look for a direct `identifier` or `field_identifier` child — covers Rust
///    `method_call_expression` (`name` is a direct identifier child) and Java
///    `method_invocation` (same).
/// 2. If the first child is a qualified expression (`scoped_identifier`,
///    `member_expression`, `attribute`, `field_expression`, `selector_expression`,
///    `qualified_identifier`), return the last `identifier`/`field_identifier`/
///    `property_identifier` inside it — strips the receiver/namespace and gives the
///    bare method name. Covers C++ `obj.f()` (`field_expression`, bare name in a
///    `field_identifier`) and `ns::f()` (`qualified_identifier`, bare name in the
///    rightmost `identifier`).
fn call_callee(node: &Node, source: &str) -> Option<String> {
    const ID_KINDS: &[&str] = &["identifier", "field_identifier", "property_identifier"];
    const QUALIFIED_KINDS: &[&str] = &[
        "scoped_identifier",
        "member_expression",
        "attribute",
        "field_expression",
        "selector_expression",
        "qualified_identifier",
    ];

    // Pass 1: direct identifier child
    let direct: Vec<String> = {
        let mut c = node.walk();
        node.children(&mut c)
            .filter(|n| ID_KINDS.contains(&n.kind()))
            .map(|n| source[n.byte_range()].trim().to_owned())
            .collect()
    };
    if let Some(name) = direct.last() {
        return Some(name.clone());
    }

    // Pass 2: identifier inside a qualified expression child
    {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if QUALIFIED_KINDS.contains(&child.kind()) {
                let mut ic = child.walk();
                let last = child
                    .children(&mut ic)
                    .filter(|n| ID_KINDS.contains(&n.kind()))
                    .last()
                    .map(|n| source[n.byte_range()].trim().to_owned());
                if last.is_some() {
                    return last;
                }
            }
        }
    }

    None
}

/// Top-level node kinds that may carry heritage (extends/implements) info, by language
/// name. Empty for languages with no inheritance concept (Go's interface satisfaction is
/// structural, not declared; C has no classes).
fn heritage_kinds_for(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &["impl_item"],
        "python" => &["class_definition"],
        "javascript" | "typescript" | "tsx" => &["class_declaration"],
        "java" => &["class_declaration", "interface_declaration"],
        "cpp" => &["class_specifier", "struct_specifier"],
        _ => &[],
    }
}

/// Extract `extends`/`implements` edges (2.2) from class/impl/interface nodes. File-level,
/// like every other edge kind here — not "type X extends Y" but "this file extends Y",
/// consistent with `defines`/`calls` being file-scoped and resolved against definers at
/// query time (see `store::edges::resolve_call`).
///
/// Field/child shapes below are verified against real tree-sitter output (`Node::to_sexp`),
/// not assumed from grammar docs:
///   Rust        `impl_item { trait: type_identifier, type: type_identifier }` — `trait` is
///               present only for `impl Trait for Type` (absent for a plain `impl Type`).
///   Python      `class_definition { superclasses: argument_list(identifier*) }`.
///   JavaScript  `class_declaration { .. class_heritage(identifier) .. }` (unlabeled child;
///               no `implements` keyword in JS).
///   TypeScript  `class_declaration { .. class_heritage(extends_clause{value:identifier}?
///               implements_clause(type_identifier*)?) .. }`.
///   Java        `class_declaration { superclass: superclass(type_identifier),
///               interfaces: super_interfaces(type_list(type_identifier*)) }`;
///               `interface_declaration { .. extends_interfaces(type_list(type_identifier*)) .. }`.
///   C++         `class_specifier|struct_specifier { .. base_class_clause(access_specifier|
///               type_identifier, ..) .. }` — access_specifier (public/private/protected)
///               nodes are filtered out, keeping only type_identifier children.
fn extract_heritage(
    node: Node,
    source: &str,
    path: &Path,
    language: &str,
    kinds: &[&str],
    edges: &mut Vec<Edge>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            match language {
                "rust" => {
                    if let Some(t) = child.child_by_field_name("trait") {
                        push_heritage(edges, path, "implements", &source[t.byte_range()]);
                    }
                }
                "python" => {
                    if let Some(bases) = child.child_by_field_name("superclasses") {
                        let mut c2 = bases.walk();
                        for base in bases.children(&mut c2) {
                            if base.kind() == "identifier" {
                                push_heritage(edges, path, "extends", &source[base.byte_range()]);
                            }
                        }
                    }
                }
                "javascript" => {
                    if let Some(heritage) = find_child_by_kind(&child, "class_heritage") {
                        let mut c2 = heritage.walk();
                        for n in heritage.children(&mut c2) {
                            if n.kind() == "identifier" {
                                push_heritage(edges, path, "extends", &source[n.byte_range()]);
                            }
                        }
                    }
                }
                "typescript" | "tsx" => {
                    if let Some(heritage) = find_child_by_kind(&child, "class_heritage") {
                        let mut c2 = heritage.walk();
                        for n in heritage.children(&mut c2) {
                            match n.kind() {
                                "extends_clause" => {
                                    if let Some(v) = n.child_by_field_name("value") {
                                        push_heritage(
                                            edges,
                                            path,
                                            "extends",
                                            &source[v.byte_range()],
                                        );
                                    }
                                }
                                "implements_clause" => {
                                    let mut c3 = n.walk();
                                    for t in n.children(&mut c3) {
                                        if t.kind() == "type_identifier" {
                                            push_heritage(
                                                edges,
                                                path,
                                                "implements",
                                                &source[t.byte_range()],
                                            );
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "java" => {
                    if child.kind() == "class_declaration" {
                        if let Some(superclass) = child.child_by_field_name("superclass") {
                            if let Some(t) = find_child_by_kind(&superclass, "type_identifier") {
                                push_heritage(edges, path, "extends", &source[t.byte_range()]);
                            }
                        }
                        if let Some(interfaces) = child.child_by_field_name("interfaces") {
                            extract_java_type_list(&interfaces, source, path, "implements", edges);
                        }
                    } else if child.kind() == "interface_declaration" {
                        if let Some(ext) = find_child_by_kind(&child, "extends_interfaces") {
                            extract_java_type_list(&ext, source, path, "extends", edges);
                        }
                    }
                }
                "cpp" => {
                    if let Some(base_clause) = find_child_by_kind(&child, "base_class_clause") {
                        let mut c2 = base_clause.walk();
                        for n in base_clause.children(&mut c2) {
                            if n.kind() == "type_identifier" {
                                push_heritage(edges, path, "extends", &source[n.byte_range()]);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        extract_heritage(child, source, path, language, kinds, edges);
    }
}

/// Push an `extends`/`implements` heritage edge (no symbol_kind/line_range — those are
/// `defines`-only, see [`Edge`]). Skips an empty name (a malformed/partial parse).
fn push_heritage(edges: &mut Vec<Edge>, path: &Path, kind: &'static str, name: &str) {
    if name.is_empty() {
        return;
    }
    edges.push(Edge {
        from: path.to_path_buf(),
        kind,
        to: name.to_owned(),
        symbol_kind: None,
        line_range: None,
    });
}

/// First direct child of `node` with tree-sitter kind `kind` — for constructs not exposed
/// as a named field (JS/TS `class_heritage`, C++ `base_class_clause`, Java's unlabeled
/// `extends_interfaces`, the `type_identifier` inside Java's `superclass` wrapper node).
// Not `Iterator::find`: the adaptor borrows `cursor`, which doesn't live past this
// statement, so the NLL borrow checker rejects the chained form here.
#[allow(clippy::manual_find)]
fn find_child_by_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Java `super_interfaces`/`extends_interfaces` both wrap a `type_list` of `type_identifier`
/// children — shared walk for both (`interfaces:` on a class, and the unlabeled child on an
/// `interface_declaration`).
fn extract_java_type_list(
    wrapper: &Node,
    source: &str,
    path: &Path,
    kind: &'static str,
    edges: &mut Vec<Edge>,
) {
    let Some(type_list) = find_child_by_kind(wrapper, "type_list") else {
        return;
    };
    let mut cursor = type_list.walk();
    for t in type_list.children(&mut cursor) {
        if t.kind() == "type_identifier" {
            push_heritage(edges, path, kind, &source[t.byte_range()]);
        }
    }
}

/// Tree-sitter node kinds that represent an import/use statement, by language name. An
/// empty slice means imports aren't extracted for that language (its `defines` edges are
/// still emitted). Go uses `import_spec` (one per import) so grouped `import ( … )` blocks
/// yield an edge per line.
fn import_kinds_for(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &["use_declaration"],
        "python" => &["import_statement", "import_from_statement"],
        "javascript" | "typescript" | "tsx" => &["import_statement"],
        "go" => &["import_spec"],
        "java" => &["import_declaration"],
        "c" | "cpp" => &["preproc_include"],
        _ => &[],
    }
}

/// Walk the tree and push a `defines` edge per named symbol node (functions, types,
/// classes, …). Recurses through everything so nested symbols (impl methods, class
/// methods, exported declarations) are captured too. `symbol_name` returns the node kind
/// as a sentinel when no name child exists (anonymous/wrapper nodes like
/// `export_statement` / `decorated_definition`) — those are skipped, and the real name is
/// found when recursion reaches the inner declaration. Deduped by name.
fn extract_defines(
    node: Node,
    source: &str,
    path: &Path,
    kinds: &[&str],
    edges: &mut Vec<Edge>,
    seen: &mut std::collections::HashSet<String>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            let name = symbol_name(&child, source);
            if name != child.kind() && seen.insert(name.clone()) {
                // 1-based, inclusive line range (tree-sitter positions are 0-based rows).
                let start_line = child.start_position().row + 1;
                let end_line = child.end_position().row + 1;
                edges.push(Edge {
                    from: path.to_path_buf(),
                    kind: "defines",
                    to: name,
                    symbol_kind: Some(simplify_symbol_kind(child.kind())),
                    line_range: Some((start_line, end_line)),
                });
            }
        }
        extract_defines(child, source, path, kinds, edges, seen);
    }
}

/// Map a tree-sitter top-level node kind to a compact, cross-language symbol-kind vocabulary
/// (2.1) — every `top_level_kinds` entry across the 8 supported languages is covered
/// explicitly; an unmapped kind (a future grammar/language addition) falls back to `"other"`
/// rather than leaking a grammar-specific node-kind string into the `symbols` table.
pub(crate) fn simplify_symbol_kind(node_kind: &str) -> &'static str {
    match node_kind {
        "function_item" | "function_definition" | "function_declaration" => "fn",
        "method_declaration" => "method",
        "impl_item" => "impl",
        "struct_item" | "struct_specifier" => "struct",
        "enum_item" | "enum_specifier" | "enum_declaration" => "enum",
        "union_specifier" => "union",
        "trait_item" => "trait",
        "class_declaration" | "class_definition" | "class_specifier" => "class",
        "interface_declaration" => "interface",
        "mod_item" | "namespace_definition" => "module",
        "type_alias" | "type_alias_declaration" | "type_declaration" | "type_definition" => "type",
        "const_item" | "const_declaration" => "const",
        "static_item" => "static",
        "var_declaration" | "variable_declaration" | "lexical_declaration" => "var",
        "template_declaration" => "template",
        "preproc_def" | "preproc_function_def" => "macro",
        "declaration" => "declaration",
        _ => "other",
    }
}

/// Walk the tree for import/use nodes and push an `imports` edge per resolved module path.
/// Recurses through non-import nodes so imports nested in `mod` blocks or functions are
/// still found; it does not descend into a matched import node.
fn extract_imports(node: Node, source: &str, path: &Path, kinds: &[&str], edges: &mut Vec<Edge>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            if let Some(target) = import_target(&child, source) {
                if !target.is_empty() {
                    edges.push(Edge {
                        from: path.to_path_buf(),
                        kind: "imports",
                        to: target,
                        symbol_kind: None,
                        line_range: None,
                    });
                }
            }
        } else {
            extract_imports(child, source, path, kinds, edges);
        }
    }
}

/// Resolve the imported module/path from an import node: prefer a string-literal module
/// path (JS/TS `from "x"`, Go `import_spec "fmt"`, C/C++ `#include "foo.h"`) or a C/C++
/// `system_lib_string` (`#include <stdio.h>`, stripped of `<>`), else the dotted/scoped
/// name (Python `import a.b`, Rust `use a::b`, Java `import a.b`). Returns the primary
/// target; grouped multi-name imports (e.g. Python `import a, b`) surface their first
/// module in D1.
fn import_target(node: &Node, source: &str) -> Option<String> {
    // C/C++ `#include <stdio.h>` — the header lives in a `system_lib_string` leaf whose
    // text includes the angle brackets; strip them.
    if let Some(s) = find_descendant(node, &["system_lib_string"]) {
        let raw = &source[s.byte_range()];
        return Some(raw.trim_matches(|c| c == '<' || c == '>').to_owned());
    }
    const STRING_KINDS: &[&str] = &[
        "string",
        "string_literal",
        "interpreted_string_literal",
        "raw_string_literal",
    ];
    if let Some(s) = find_descendant(node, STRING_KINDS) {
        let raw = &source[s.byte_range()];
        return Some(
            raw.trim_matches(|c| c == '"' || c == '\'' || c == '`')
                .to_owned(),
        );
    }
    const NAME_KINDS: &[&str] = &[
        "dotted_name",
        "scoped_identifier",
        "scoped_use_list",
        "use_list",
        "identifier",
    ];
    find_descendant(node, NAME_KINDS).map(|n| source[n.byte_range()].trim().to_owned())
}

/// First descendant (pre-order) whose kind is in `kinds`.
fn find_descendant<'a>(node: &Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return Some(child);
        }
        if let Some(found) = find_descendant(&child, kinds) {
            return Some(found);
        }
    }
    None
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
            if text.len() < MIN_CHUNK_BYTES {
                continue;
            }
            if text.len() > MAX_CHUNK_BYTES {
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
///
/// Most languages put the symbol name in a direct `identifier`/`name`/`type_identifier`
/// child. C/C++ instead nest it inside a declarator chain
/// (`function_definition → function_declarator → identifier`, possibly wrapped in
/// `pointer_declarator`/`reference_declarator`/`parenthesized_declarator`), so we descend
/// through the `declarator` field when no direct name child exists.
fn symbol_name(node: &Node, source: &str) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "name" || k == "type_identifier" || k == "field_identifier" {
            return source[child.byte_range()].to_owned();
        }
    }
    // C/C++ declarator chain: follow `declarator` fields down to the inner name.
    if let Some(name) = c_declarator_name(node, source) {
        return name;
    }
    node.kind().to_owned()
}

/// Follow a C/C++ declarator chain (`function_declarator`, `pointer_declarator`,
/// `reference_declarator`, `parenthesized_declarator`, `init_declarator`, …) down to the
/// innermost `identifier`/`field_identifier`/`type_identifier`/`qualified_identifier`,
/// which is the declared symbol's bare name. Returns `None` when there is no declarator.
fn c_declarator_name(node: &Node, source: &str) -> Option<String> {
    const NAME_KINDS: &[&str] = &[
        "identifier",
        "field_identifier",
        "type_identifier",
        "qualified_identifier",
    ];
    let decl = node.child_by_field_name("declarator")?;
    // The name is the first descendant name-kind node within the declarator subtree.
    let name_node = if NAME_KINDS.contains(&decl.kind()) {
        decl
    } else {
        find_descendant(&decl, NAME_KINDS)?
    };
    Some(name_text(&name_node, source))
}

/// Text of a name node, collapsing `qualified_identifier` (`A::B::f`) to its rightmost
/// bare `identifier` (`f`). Other name kinds are returned verbatim.
fn name_text(node: &Node, source: &str) -> String {
    if node.kind() == "qualified_identifier" {
        let mut c = node.walk();
        if let Some(last) = node
            .children(&mut c)
            .filter(|n| n.kind() == "identifier")
            .last()
        {
            return source[last.byte_range()].trim().to_owned();
        }
    }
    source[node.byte_range()].trim().to_owned()
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

    /// Regression test: a source file with one stray non-UTF-8 byte (e.g. in a comment or
    /// string literal) must still be indexed (lossily), not hard-error the whole file — matches
    /// every other text-shaped parser's default `[parsers] encoding = "auto"` behavior.
    #[test]
    fn parses_source_file_with_invalid_utf8_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.rs");
        let mut bytes = b"// Caf".to_vec();
        bytes.push(0xE9); // Latin-1 "e"-acute — invalid on its own as UTF-8.
        bytes.extend_from_slice(b" note\npub fn hello() -> i32 { 42 }\n");
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "fixture must actually be invalid UTF-8"
        );
        std::fs::write(&p, &bytes).unwrap();

        let extracted = CodeParser.parse(&p).unwrap();
        assert!(!extracted.chunks.is_empty());
        assert!(extracted.chunks.iter().any(|c| c.text.contains("hello")));
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

    fn imports_of(ex: &Extracted) -> Vec<&str> {
        ex.edges
            .iter()
            .filter(|e| e.kind == "imports")
            .map(|e| e.to.as_str())
            .collect()
    }
    fn defines_of(ex: &Extracted) -> Vec<&str> {
        ex.edges
            .iter()
            .filter(|e| e.kind == "defines")
            .map(|e| e.to.as_str())
            .collect()
    }

    #[test]
    fn edges_rust_imports_and_defines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.rs");
        std::fs::write(
            &p,
            "use std::collections::HashMap;\nuse crate::foo::Bar;\n\npub fn run() {}\npub struct Widget { x: i32 }\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let imports = imports_of(&ex);
        let defines = defines_of(&ex);
        assert!(
            imports
                .iter()
                .any(|i| i.contains("std::collections::HashMap")),
            "imports: {imports:?}"
        );
        assert!(
            imports.iter().any(|i| i.contains("crate::foo::Bar")),
            "imports: {imports:?}"
        );
        assert!(defines.contains(&"run"), "defines: {defines:?}");
        assert!(defines.contains(&"Widget"), "defines: {defines:?}");
        // Every edge originates at the parsed file.
        assert!(ex.edges.iter().all(|e| e.from == p));
    }

    fn heritage_of(ex: &crate::types::Extracted) -> Vec<(&str, &str)> {
        ex.edges
            .iter()
            .filter(|e| e.kind == "extends" || e.kind == "implements")
            .map(|e| (e.kind, e.to.as_str()))
            .collect()
    }

    #[test]
    fn heritage_rust_impl_trait_for_type() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.rs");
        std::fs::write(&p, "impl Foo for Bar {}\nimpl Bar {}\n").unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let h = heritage_of(&ex);
        assert_eq!(h, vec![("implements", "Foo")], "{h:?}");
    }

    #[test]
    fn heritage_python_multiple_base_classes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.py");
        std::fs::write(&p, "class Foo(Base1, Base2):\n    pass\n").unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let h = heritage_of(&ex);
        assert_eq!(h, vec![("extends", "Base1"), ("extends", "Base2")], "{h:?}");
    }

    #[test]
    fn heritage_javascript_extends_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.js");
        std::fs::write(&p, "class Foo extends Bar {}\n").unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        assert_eq!(heritage_of(&ex), vec![("extends", "Bar")]);
    }

    #[test]
    fn heritage_typescript_extends_and_implements() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.ts");
        std::fs::write(&p, "class Foo extends Bar implements Baz, Qux {}\n").unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let h = heritage_of(&ex);
        assert_eq!(
            h,
            vec![
                ("extends", "Bar"),
                ("implements", "Baz"),
                ("implements", "Qux")
            ],
            "{h:?}"
        );
    }

    #[test]
    fn heritage_java_class_and_interface() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("A.java");
        std::fs::write(
            &p,
            "class Foo extends Bar implements Baz, Qux {}\ninterface A extends B, C {}\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let h = heritage_of(&ex);
        assert_eq!(
            h,
            vec![
                ("extends", "Bar"),
                ("implements", "Baz"),
                ("implements", "Qux"),
                ("extends", "B"),
                ("extends", "C"),
            ],
            "{h:?}"
        );
    }

    #[test]
    fn heritage_cpp_class_and_struct_base_clause() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.cpp");
        std::fs::write(
            &p,
            "class Foo : public Bar, private Baz {};\nstruct S : Base {};\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let h = heritage_of(&ex);
        assert_eq!(
            h,
            vec![("extends", "Bar"), ("extends", "Baz"), ("extends", "Base"),],
            "{h:?}"
        );
    }

    #[test]
    fn heritage_absent_for_go_and_c() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("a.go");
        std::fs::write(&go, "package main\n\nfunc main() {}\n").unwrap();
        assert!(heritage_of(&CodeParser.parse(&go).unwrap()).is_empty());

        let c = dir.path().join("a.c");
        std::fs::write(&c, "struct Foo { int x; };\n").unwrap();
        assert!(heritage_of(&CodeParser.parse(&c).unwrap()).is_empty());
    }

    #[test]
    fn defines_edges_carry_symbol_kind_and_line_range() {
        // 2.1: `defines` edges now carry a compact symbol kind + 1-based line range;
        // imports/calls edges carry neither.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.rs");
        std::fs::write(
            &p,
            "use std::fmt;\n\npub fn run() {\n    helper();\n}\n\npub struct Widget {\n    x: i32,\n}\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();

        let run_edge = ex
            .edges
            .iter()
            .find(|e| e.kind == "defines" && e.to == "run")
            .expect("run defines edge");
        assert_eq!(run_edge.symbol_kind, Some("fn"));
        // `pub fn run() {` starts on line 3, its closing `}` is line 5 (1-based).
        assert_eq!(run_edge.line_range, Some((3, 5)));

        let widget_edge = ex
            .edges
            .iter()
            .find(|e| e.kind == "defines" && e.to == "Widget")
            .expect("Widget defines edge");
        assert_eq!(widget_edge.symbol_kind, Some("struct"));
        assert_eq!(widget_edge.line_range, Some((7, 9)));

        // Imports and calls never carry symbol metadata.
        assert!(ex
            .edges
            .iter()
            .filter(|e| e.kind != "defines")
            .all(|e| e.symbol_kind.is_none() && e.line_range.is_none()));
    }

    #[test]
    fn simplify_symbol_kind_covers_every_top_level_kind() {
        // Every top_level_kinds entry across all 8 languages must map to something other
        // than the "other" fallback — this pins the mapping table against silent gaps
        // when a language's top_level_kinds list changes.
        const ALL_TOP_LEVEL_KINDS: &[&str] = &[
            // Rust
            "function_item",
            "impl_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "mod_item",
            "type_alias",
            "const_item",
            "static_item",
            // Python
            "function_definition",
            "class_definition",
            // JS/TS/TSX
            "function_declaration",
            "class_declaration",
            "lexical_declaration",
            "variable_declaration",
            "interface_declaration",
            "type_alias_declaration",
            // Go
            "method_declaration",
            "type_declaration",
            "const_declaration",
            "var_declaration",
            // Java
            "enum_declaration",
            // C/C++
            "struct_specifier",
            "enum_specifier",
            "union_specifier",
            "type_definition",
            "preproc_def",
            "preproc_function_def",
            "class_specifier",
            "namespace_definition",
            "template_declaration",
            "declaration",
        ];
        for kind in ALL_TOP_LEVEL_KINDS {
            assert_ne!(
                simplify_symbol_kind(kind),
                "other",
                "unmapped top_level_kinds entry: {kind}"
            );
        }
        // A genuinely unknown kind still falls back gracefully.
        assert_eq!(simplify_symbol_kind("some_future_grammar_node"), "other");
    }

    #[test]
    fn edges_python_imports() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.py");
        std::fs::write(
            &p,
            "import os\nfrom collections import OrderedDict\n\ndef f():\n    return os.getpid()\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let imports = imports_of(&ex);
        assert!(imports.contains(&"os"), "py imports: {imports:?}");
        assert!(
            imports.iter().any(|i| i.contains("collections")),
            "py imports: {imports:?}"
        );
    }

    #[test]
    fn edges_javascript_imports() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.js");
        std::fs::write(
            &p,
            "import { x } from './util';\nimport React from 'react';\nfunction go() {}\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let imports = imports_of(&ex);
        assert!(imports.contains(&"./util"), "js imports: {imports:?}");
        assert!(imports.contains(&"react"), "js imports: {imports:?}");
    }

    #[test]
    fn edges_go_grouped_imports() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.go");
        std::fs::write(
            &p,
            "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() { fmt.Println(os.Args) }\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let imports = imports_of(&ex);
        // Grouped imports must each yield an edge (we match import_spec, not the block).
        assert!(imports.contains(&"fmt"), "go imports: {imports:?}");
        assert!(imports.contains(&"os"), "go imports: {imports:?}");
    }

    fn calls_of(ex: &Extracted) -> Vec<&str> {
        ex.edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| e.to.as_str())
            .collect()
    }

    #[test]
    fn edges_calls_rust() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main.rs");
        std::fs::write(
            &p,
            "fn main() { parse(\"a\"); render(); obj.connect(); }\n\
             fn parse(s: &str) -> i32 { s.len() as i32 }\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let calls = calls_of(&ex);
        assert!(calls.contains(&"parse"), "rust calls: {calls:?}");
        assert!(calls.contains(&"render"), "rust calls: {calls:?}");
        assert!(calls.contains(&"connect"), "rust calls: {calls:?}");
        // Deduped — parse called once but only one edge
        assert_eq!(calls.iter().filter(|&&c| c == "parse").count(), 1);
    }

    #[test]
    fn edges_calls_python() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.py");
        std::fs::write(
            &p,
            "import os\ndef run():\n    parse(data)\n    render(ctx)\n    obj.save()\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let calls = calls_of(&ex);
        assert!(calls.contains(&"parse"), "python calls: {calls:?}");
        assert!(calls.contains(&"render"), "python calls: {calls:?}");
        assert!(calls.contains(&"save"), "python calls: {calls:?}");
    }

    #[test]
    fn edges_calls_javascript() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.js");
        std::fs::write(
            &p,
            "const x = parse(data);\nobj.render(ctx);\nconst y = build();\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        let calls = calls_of(&ex);
        assert!(calls.contains(&"parse"), "js calls: {calls:?}");
        assert!(calls.contains(&"render"), "js calls: {calls:?}");
        assert!(calls.contains(&"build"), "js calls: {calls:?}");
    }

    #[test]
    fn code_parser_accepts_c_and_cpp_extensions() {
        let parser = CodeParser;
        for ext in ["c", "h"] {
            assert!(
                parser.accepts_path(Path::new(&format!("file.{ext}"))),
                "should accept .{ext}"
            );
        }
        for ext in ["cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++"] {
            assert!(
                parser.accepts_path(Path::new(&format!("file.{ext}"))),
                "should accept .{ext}"
            );
        }
    }

    #[test]
    fn parses_c_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main.c");
        std::fs::write(
            &p,
            "#include <stdio.h>\nint add(int a, int b) { return a + b; }\nint main(void) { printf(\"%d\\n\", add(1, 2)); return 0; }\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        assert!(!ex.chunks.is_empty());
        assert_eq!(ex.chunks[0].language.as_deref(), Some("c"));
    }

    #[test]
    fn edges_calls_and_imports_c() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.c");
        std::fs::write(
            &p,
            "#include <stdio.h>\n#include \"helper.h\"\n\
             int run(void) {\n\
             \x20   printf(\"hi\");\n\
             \x20   parse(buf);\n\
             \x20   return 0;\n\
             }\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();

        let imports = imports_of(&ex);
        // `<stdio.h>` → stripped of angle brackets; `"helper.h"` → stripped of quotes.
        assert!(imports.contains(&"stdio.h"), "c imports: {imports:?}");
        assert!(imports.contains(&"helper.h"), "c imports: {imports:?}");

        let calls = calls_of(&ex);
        assert!(calls.contains(&"printf"), "c calls: {calls:?}");
        assert!(calls.contains(&"parse"), "c calls: {calls:?}");

        // The function definition yields a `defines` edge (declarator-nested name).
        let defines = defines_of(&ex);
        assert!(defines.contains(&"run"), "c defines: {defines:?}");

        assert!(ex.edges.iter().all(|e| e.from == p));
    }

    #[test]
    fn parses_cpp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main.cpp");
        std::fs::write(
            &p,
            "#include <vector>\nclass Greeter { public: void greet(); };\nvoid Greeter::greet() {}\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();
        assert!(!ex.chunks.is_empty());
        assert_eq!(ex.chunks[0].language.as_deref(), Some("cpp"));
    }

    #[test]
    fn edges_calls_and_imports_cpp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.cpp");
        std::fs::write(
            &p,
            "#include <vector>\n#include \"foo.hpp\"\n\
             int run() {\n\
             \x20   Obj obj;\n\
             \x20   obj.render();\n\
             \x20   ns::build();\n\
             \x20   compute();\n\
             \x20   return 0;\n\
             }\n",
        )
        .unwrap();
        let ex = CodeParser.parse(&p).unwrap();

        let imports = imports_of(&ex);
        assert!(imports.contains(&"vector"), "cpp imports: {imports:?}");
        assert!(imports.contains(&"foo.hpp"), "cpp imports: {imports:?}");

        let calls = calls_of(&ex);
        // Bare call.
        assert!(calls.contains(&"compute"), "cpp calls: {calls:?}");
        // Method call `obj.render()` → bare method name (field_expression).
        assert!(calls.contains(&"render"), "cpp calls: {calls:?}");
        // Qualified call `ns::build()` → rightmost name (qualified_identifier).
        assert!(calls.contains(&"build"), "cpp calls: {calls:?}");

        assert!(ex.edges.iter().all(|e| e.from == p));
    }
}
