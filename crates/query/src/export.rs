//! Context-export renderers: XML (primary), Markdown, JSON.
//!
//! XML is recommended as the primary format because Anthropic's prompt-engineering docs
//! specify XML tags as the preferred structural delimiter for LLM context windows.
//! (<https://docs.anthropic.com/en/docs/build-with-claude/prompt-engineering/use-xml-tags>)

use anyhow::{Context, Result};
use indexa_core::store::{
    abstract_from, ChunkRecord, CodeGraph, Store, SummaryRecord, WeightRecord,
};
use std::path::Path;

/// Rough token estimate for a rendered string: ~4 chars per token (the common heuristic
/// for English/code). Lets a user gauge an export's size against a token-limited AI tool
/// before pasting it. Deliberately approximate — not a tokenizer.
pub fn approx_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// One node in the in-memory export tree.
pub struct ExportNode {
    pub record: SummaryRecord,
    pub children: Vec<ExportNode>,
}

/// The L0 one-line abstract for a node: the stored `summary_l0`, or derived from
/// the L1 summary as a fallback (older rows / records built without an abstract).
fn node_abstract(node: &ExportNode) -> String {
    node.record
        .summary_l0
        .clone()
        .unwrap_or_else(|| abstract_from(&node.record.summary))
}

/// Build an export tree rooted at `root_path`, going at most `max_depth` levels deep.
/// Walks only rows that exist in the `summaries` table.
pub fn build_tree(
    store: &Store,
    root_path: &str,
    max_depth: Option<usize>,
) -> Result<Option<ExportNode>> {
    let record = store
        .summary_by_path(root_path)
        .with_context(|| format!("reading summary for {root_path}"))?;
    let Some(record) = record else {
        return Ok(None);
    };

    fn build_inner(
        store: &Store,
        record: SummaryRecord,
        root_depth: i64,
        max_depth: Option<usize>,
    ) -> Result<ExportNode> {
        let relative = (record.depth - root_depth) as usize;
        let at_limit = max_depth.is_some_and(|md| relative >= md);
        let children = if at_limit {
            vec![]
        } else {
            let child_records = store
                .children_summaries(&record.path)
                .with_context(|| format!("children of {}", record.path))?;
            child_records
                .into_iter()
                .map(|c| build_inner(store, c, root_depth, max_depth))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(ExportNode { record, children })
    }

    let root_depth = record.depth;
    Ok(Some(build_inner(store, record, root_depth, max_depth)?))
}

/// Render the tree as XML (primary AI-context format). The `<index>` element carries an
/// `approx_tokens` estimate of the body so a user can size it before pasting into an AI tool.
pub fn render_xml(node: &ExportNode, generated_at: &str) -> String {
    let mut body = String::with_capacity(4096);
    render_xml_node(node, &mut body, 1);
    let mut out = String::with_capacity(body.len() + 128);
    out.push_str(&format!(
        r#"<index root="{}" generated_at="{}" approx_tokens="{}">"#,
        xml_attr(&node.record.path),
        xml_attr(generated_at),
        approx_tokens(&body),
    ));
    out.push('\n');
    out.push_str(&body);
    out.push_str("</index>\n");
    out
}

fn render_xml_node(node: &ExportNode, out: &mut String, indent: usize) {
    let pad = "  ".repeat(indent);
    let tag = if node.record.kind == "dir" {
        "directory"
    } else {
        "file"
    };
    let path_name = Path::new(&node.record.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| node.record.path.clone());
    out.push_str(&format!(
        r#"{pad}<{tag} path="{}" name="{}" depth="{}">"#,
        xml_attr(&node.record.path),
        xml_attr(&path_name),
        node.record.depth,
    ));
    out.push('\n');
    out.push_str(&format!(
        "{pad}  <abstract>{}</abstract>\n",
        xml_text(&node_abstract(node))
    ));
    out.push_str(&format!(
        "{pad}  <summary>{}</summary>\n",
        xml_text(&node.record.summary)
    ));
    if !node.children.is_empty() {
        out.push_str(&format!(
            "{pad}  <children count=\"{}\">\n",
            node.children.len()
        ));
        for child in &node.children {
            render_xml_node(child, out, indent + 2);
        }
        out.push_str(&format!("{pad}  </children>\n"));
    }
    out.push_str(&format!("{pad}</{tag}>\n"));
}

/// Render the tree as Markdown (suitable for chat UIs that strip XML).
pub fn render_markdown(node: &ExportNode) -> String {
    let mut body = String::with_capacity(4096);
    render_md_node(node, &mut body, 2);
    let mut out = String::with_capacity(body.len() + 128);
    out.push_str(&format!("# Index: {}\n\n", node.record.path));
    out.push_str(&format!(
        "> ~{} tokens (estimate, chars/4)\n\n",
        approx_tokens(&body)
    ));
    out.push_str(&body);
    out
}

fn render_md_node(node: &ExportNode, out: &mut String, level: usize) {
    let prefix = "#".repeat(level.min(6));
    let name = Path::new(&node.record.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| node.record.path.clone());
    let icon = if node.record.kind == "dir" {
        "📁"
    } else {
        "📄"
    };
    out.push_str(&format!("{prefix} {icon} {name}\n\n"));
    out.push_str(&format!("`{}`\n\n", node.record.path));
    out.push_str(&format!("**Abstract:** {}\n\n", node_abstract(node)));
    out.push_str(&format!("{}\n\n", node.record.summary));
    for child in &node.children {
        render_md_node(child, out, level + 1);
    }
}

/// Render the tree as JSON for programmatic piping.
pub fn render_json(node: &ExportNode) -> String {
    let mut out = String::with_capacity(4096);
    render_json_node(node, &mut out, 0);
    out.push('\n');
    out
}

fn render_json_node(node: &ExportNode, out: &mut String, indent: usize) {
    let pad = "  ".repeat(indent);
    let inner = "  ".repeat(indent + 1);
    out.push_str(&format!("{pad}{{\n"));
    out.push_str(&format!(
        "{inner}\"path\": {},\n",
        json_str(&node.record.path)
    ));
    out.push_str(&format!(
        "{inner}\"kind\": {},\n",
        json_str(&node.record.kind)
    ));
    out.push_str(&format!("{inner}\"depth\": {},\n", node.record.depth));
    out.push_str(&format!(
        "{inner}\"abstract\": {},\n",
        json_str(&node_abstract(node))
    ));
    out.push_str(&format!(
        "{inner}\"summary\": {}",
        json_str(&node.record.summary)
    ));
    if !node.children.is_empty() {
        out.push_str(",\n");
        out.push_str(&format!("{inner}\"children\": [\n"));
        for (i, child) in node.children.iter().enumerate() {
            render_json_node(child, out, indent + 2);
            if i + 1 < node.children.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str(&format!("{inner}]\n"));
    } else {
        out.push('\n');
    }
    out.push_str(&format!("{pad}}}"));
}

// ── Optional appended sections (--include-weights / --include-graph) ────────────

/// Render the importance-weights as a `format`-appropriate section appended to an export,
/// so an AI tool sees which files the user considers most important. Empty input → "".
pub fn render_weights(weights: &[WeightRecord], format: &str) -> String {
    if weights.is_empty() {
        return String::new();
    }
    match format {
        "md" | "markdown" => {
            let mut out = String::from("\n## Importance weights\n\n| target | kind | weight | reason |\n|---|---|---|---|\n");
            for w in weights {
                out.push_str(&format!(
                    "| {} | {} | {:.2} | {} |\n",
                    w.target,
                    w.target_kind,
                    w.weight,
                    w.reason.as_deref().unwrap_or("")
                ));
            }
            out
        }
        "json" => {
            let items: Vec<String> = weights
                .iter()
                .map(|w| {
                    format!(
                        "{{\"target\": {}, \"kind\": {}, \"weight\": {:.4}, \"reason\": {}}}",
                        json_str(&w.target),
                        json_str(&w.target_kind),
                        w.weight,
                        json_str(w.reason.as_deref().unwrap_or(""))
                    )
                })
                .collect();
            format!("{{\"weights\": [{}]}}\n", items.join(", "))
        }
        _ => {
            let mut out = String::from("<weights>\n");
            for w in weights {
                out.push_str(&format!(
                    r#"  <weight target="{}" kind="{}" value="{:.2}">{}</weight>"#,
                    xml_attr(&w.target),
                    xml_attr(&w.target_kind),
                    w.weight,
                    xml_text(w.reason.as_deref().unwrap_or(""))
                ));
                out.push('\n');
            }
            out.push_str("</weights>\n");
            out
        }
    }
}

/// Render the file-to-file call graph as a `format`-appropriate section appended to an export,
/// so an AI tool sees how the exported files relate. Empty graph → "".
pub fn render_graph(graph: &CodeGraph, format: &str) -> String {
    if graph.edges.is_empty() {
        return String::new();
    }
    let base = |p: &str| {
        Path::new(p)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_owned())
    };
    match format {
        "md" | "markdown" => {
            let mut out = String::from("\n## Call graph\n\n");
            if graph.truncated {
                out.push_str("_(truncated — heaviest edges shown)_\n\n");
            }
            for e in &graph.edges {
                out.push_str(&format!(
                    "- {} → {} [{}]\n",
                    base(&e.from),
                    base(&e.to),
                    e.weight
                ));
            }
            out
        }
        "json" => {
            let edges: Vec<String> = graph
                .edges
                .iter()
                .map(|e| {
                    format!(
                        "{{\"from\": {}, \"to\": {}, \"weight\": {}}}",
                        json_str(&e.from),
                        json_str(&e.to),
                        e.weight
                    )
                })
                .collect();
            format!(
                "{{\"code_graph\": {{\"truncated\": {}, \"edges\": [{}]}}}}\n",
                graph.truncated,
                edges.join(", ")
            )
        }
        _ => {
            let mut out = format!(
                "<code-graph edges=\"{}\" truncated=\"{}\">\n",
                graph.edges.len(),
                graph.truncated
            );
            for e in &graph.edges {
                out.push_str(&format!(
                    r#"  <edge from="{}" to="{}" weight="{}"/>"#,
                    xml_attr(&e.from),
                    xml_attr(&e.to),
                    e.weight
                ));
                out.push('\n');
            }
            out.push_str("</code-graph>\n");
            out
        }
    }
}

// ── Signatures (code-skeleton) export ───────────────────────────────────────────

/// Extract a compact signature from a code chunk's full body: leading doc/comment lines plus the
/// declaration up to the first `{` (C-family) or trailing `:` (Python), with the body elided.
/// Capped at a few lines so a brace-less chunk can't dump everything. Heuristic and line-based
/// (not a parser) — label it as such in any UI. `keep_docs=false` drops the leading comment lines.
fn extract_signature(text: &str, keep_docs: bool) -> String {
    const MAX_LINES: usize = 8;
    let is_doc = |t: &str| {
        t.starts_with("///")
            || t.starts_with("//!")
            || t.starts_with("//")
            || t.starts_with('#') // Rust attrs (#[...]) + Python/shell comments
            || t.starts_with("/*")
            || t.starts_with('*')
            || t.starts_with("\"\"\"")
            || t.starts_with("'''")
    };
    let mut out: Vec<&str> = Vec::new();
    let mut in_decl = false;
    for line in text.lines() {
        let t = line.trim_start();
        if !in_decl && is_doc(t) {
            if keep_docs && !t.is_empty() {
                out.push(line);
                if out.len() >= MAX_LINES {
                    break;
                }
            }
            continue;
        }
        in_decl = true;
        out.push(line);
        if line.contains('{') || line.trim_end().ends_with(':') || out.len() >= MAX_LINES {
            break;
        }
    }
    let joined = out.join("\n");
    let trimmed = joined.trim_end();
    // If the declaration opened a brace, elide the body with a marker.
    if let Some(idx) = trimmed.rfind('{') {
        format!("{} … }}", &trimmed[..=idx])
    } else {
        trimmed.to_owned()
    }
}

/// Render a code-skeleton ("signatures") view: per code file, each indexed symbol's signature +
/// leading docstring, bodies elided. A heuristic, line-based compression of indexed code that
/// *complements* the prose-summary export — feed code structure to an AI tool at a fraction of the
/// tokens. `chunks` must be language-tagged code chunks, pre-sorted by `(entry_path, seq)`.
/// `keep_docs=false` strips leading comments (the `--strip-comments` flag).
pub fn render_signatures(chunks: &[ChunkRecord], format: &str, keep_docs: bool) -> String {
    // Group consecutive chunks by file (input is already path-ordered).
    let mut files: Vec<(&str, Vec<&ChunkRecord>)> = Vec::new();
    for c in chunks {
        match files.last_mut() {
            Some((p, v)) if *p == c.entry_path.as_str() => v.push(c),
            _ => files.push((c.entry_path.as_str(), vec![c])),
        }
    }

    match format {
        "md" | "markdown" => {
            let mut out = String::from("\n## Code signatures\n\n");
            out.push_str("_Heuristic skeleton — symbol signatures with bodies elided._\n\n");
            for (path, syms) in &files {
                let lang = syms
                    .first()
                    .and_then(|c| c.language.clone())
                    .unwrap_or_default();
                out.push_str(&format!("### `{path}`\n\n```{lang}\n"));
                for c in syms {
                    out.push_str(&extract_signature(&c.text, keep_docs));
                    out.push_str("\n\n");
                }
                out.push_str("```\n\n");
            }
            out
        }
        "json" => {
            let files_json: Vec<String> = files
                .iter()
                .map(|(path, syms)| {
                    let symbols: Vec<String> = syms
                        .iter()
                        .map(|c| {
                            format!(
                                "{{\"name\": {}, \"signature\": {}}}",
                                json_str(&c.heading),
                                json_str(&extract_signature(&c.text, keep_docs))
                            )
                        })
                        .collect();
                    format!(
                        "{{\"path\": {}, \"symbols\": [{}]}}",
                        json_str(path),
                        symbols.join(", ")
                    )
                })
                .collect();
            format!("{{\"signatures\": [{}]}}\n", files_json.join(", "))
        }
        _ => {
            let mut body = String::new();
            for (path, syms) in &files {
                body.push_str(&format!("  <file path=\"{}\">\n", xml_attr(path)));
                for c in syms {
                    body.push_str(&format!(
                        "    <symbol name=\"{}\">{}</symbol>\n",
                        xml_attr(&c.heading),
                        xml_text(&extract_signature(&c.text, keep_docs))
                    ));
                }
                body.push_str("  </file>\n");
            }
            format!(
                "<signatures files=\"{}\" approx_tokens=\"{}\">\n{body}</signatures>\n",
                files.len(),
                approx_tokens(&body),
            )
        }
    }
}

// ── XML helpers ───────────────────────────────────────────────────────────────

fn xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn json_str(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::store::{Store, SummaryRecord};

    fn make_record(path: &str, kind: &str, parent: Option<&str>, depth: i64) -> SummaryRecord {
        SummaryRecord {
            path: path.to_owned(),
            kind: kind.to_owned(),
            parent_path: parent.map(|s| s.to_owned()),
            depth,
            summary: format!("Summary of {path}"),
            summary_l0: Some(format!("Abstract of {path}")),
            embedding: None,
            child_count: 0,
            byte_size: 0,
            model: "test".to_owned(),
            source_hash: String::new(),
            generated_at: 0,
        }
    }

    fn make_tree() -> ExportNode {
        ExportNode {
            record: make_record("/root", "dir", None, 0),
            children: vec![
                ExportNode {
                    record: make_record("/root/src", "dir", Some("/root"), 1),
                    children: vec![ExportNode {
                        record: make_record("/root/src/main.rs", "file", Some("/root/src"), 2),
                        children: vec![],
                    }],
                },
                ExportNode {
                    record: make_record("/root/README.md", "file", Some("/root"), 1),
                    children: vec![],
                },
            ],
        }
    }

    #[test]
    fn xml_renders_valid_structure() {
        let tree = make_tree();
        let xml = render_xml(&tree, "2026-05-28");
        assert!(xml.starts_with("<index "));
        assert!(xml.ends_with("</index>\n"));
        assert!(xml.contains("<directory "));
        assert!(xml.contains("<file "));
        assert!(xml.contains("<summary>"));
        assert!(xml.contains("</summary>"));
        // Token estimate is present on the root element.
        assert!(xml.contains("approx_tokens=\""));
    }

    #[test]
    fn markdown_and_xml_carry_token_estimate() {
        let tree = make_tree();
        assert!(render_markdown(&tree).contains("tokens (estimate"));
        assert!(approx_tokens("abcdefgh") == 2); // 8 chars / 4
        assert!(approx_tokens("") == 0);
    }

    #[test]
    fn weights_and_graph_sections_render_per_format() {
        use indexa_core::store::{CodeGraph, CodeGraphEdge, CodeGraphNode, WeightRecord};
        let weights = vec![WeightRecord {
            target_kind: "file".into(),
            target: "/r/main.rs".into(),
            weight: 2.0,
            source: "user".into(),
            reason: Some("entrypoint".into()),
            updated_at: 0,
        }];
        let graph = CodeGraph {
            nodes: vec![CodeGraphNode {
                path: "/r/main.rs".into(),
                out_degree: 1,
                in_degree: 0,
                pagerank: 0.5,
            }],
            edges: vec![CodeGraphEdge {
                from: "/r/main.rs".into(),
                to: "/r/lib.rs".into(),
                weight: 3,
            }],
            truncated: false,
        };
        // XML
        assert!(render_weights(&weights, "xml").contains("<weight target=\"/r/main.rs\""));
        assert!(render_graph(&graph, "xml").contains("<edge from=\"/r/main.rs\""));
        // Markdown
        assert!(render_weights(&weights, "md").contains("## Importance weights"));
        assert!(render_graph(&graph, "md").contains("main.rs → lib.rs [3]"));
        // JSON
        assert!(render_weights(&weights, "json").contains("\"weights\""));
        assert!(render_graph(&graph, "json").contains("\"code_graph\""));
        // Empty inputs render nothing.
        assert_eq!(render_weights(&[], "xml"), "");
    }

    #[test]
    fn xml_escapes_special_chars() {
        let xml = xml_attr("a & b < c > d \"e\"");
        assert_eq!(xml, "a &amp; b &lt; c &gt; d &quot;e&quot;");
    }

    #[test]
    fn markdown_renders_headings() {
        let tree = make_tree();
        let md = render_markdown(&tree);
        assert!(md.starts_with("# Index:"));
        assert!(md.contains("📁"));
        assert!(md.contains("📄"));
    }

    #[test]
    fn json_is_parseable() {
        let tree = make_tree();
        let json_out = render_json(&tree);
        // Validate it at least starts/ends like JSON
        assert!(json_out.trim_start().starts_with('{'));
        assert!(json_out.trim_end().ends_with('}'));
        assert!(json_out.contains("\"path\""));
        assert!(json_out.contains("\"summary\""));
        assert!(json_out.contains("\"children\""));
    }

    #[test]
    fn build_tree_returns_none_for_missing_path() {
        let store = Store::open_in_memory().unwrap();
        let result = build_tree(&store, "/nonexistent", None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn build_tree_depth_limit() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&make_record("/r", "dir", None, 0))
            .unwrap();
        store
            .upsert_summary(&make_record("/r/a", "dir", Some("/r"), 1))
            .unwrap();
        store
            .upsert_summary(&make_record("/r/a/b", "file", Some("/r/a"), 2))
            .unwrap();

        let tree = build_tree(&store, "/r", Some(1)).unwrap().unwrap();
        assert_eq!(tree.children.len(), 1); // /r/a included
        assert!(tree.children[0].children.is_empty()); // /r/a/b excluded
    }

    fn code_chunk(path: &str, heading: &str, text: &str, lang: &str) -> ChunkRecord {
        ChunkRecord {
            entry_path: path.to_owned(),
            seq: 0,
            heading: heading.to_owned(),
            text: text.to_owned(),
            language: Some(lang.to_owned()),
            embedding: None,
            embed_model: None,
        }
    }

    #[test]
    fn signatures_keep_declaration_drop_body() {
        let chunks = vec![
            code_chunk(
                "/r/lib.rs",
                "compute",
                "/// Adds two numbers.\npub fn compute(a: u32, b: u32) -> u32 {\n    let s = a + b;\n    s\n}",
                "rust",
            ),
            code_chunk(
                "/r/app.py",
                "greet",
                "def greet(name):\n    return f\"hi {name}\"",
                "python",
            ),
        ];

        let md = render_signatures(&chunks, "md", true);
        assert!(
            md.contains("pub fn compute(a: u32, b: u32) -> u32 {"),
            "decl kept: {md}"
        );
        assert!(md.contains("… }"), "body elided");
        assert!(!md.contains("let s = a + b"), "rust body leaked: {md}");
        assert!(md.contains("Adds two numbers"), "docstring kept");
        assert!(md.contains("def greet(name):"));
        assert!(!md.contains("return f"), "python body leaked: {md}");

        // --strip-comments drops the docstring.
        let md_nodoc = render_signatures(&chunks, "md", false);
        assert!(
            !md_nodoc.contains("Adds two numbers"),
            "docstring should be stripped: {md_nodoc}"
        );

        // XML carries a token estimate + symbol names per file.
        let xml = render_signatures(&chunks, "xml", true);
        assert!(xml.contains("<signatures files=\"2\""));
        assert!(xml.contains("name=\"compute\""));
        assert!(xml.contains("approx_tokens=\""));

        // JSON groups symbols by file.
        let json = render_signatures(&chunks, "json", true);
        assert!(json.contains("\"signatures\""));
        assert!(json.contains("\"path\": \"/r/lib.rs\""));
    }
}
