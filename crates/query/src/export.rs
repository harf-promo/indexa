//! Context-export renderers: XML (primary), Markdown, JSON.
//!
//! XML is recommended as the primary format because Anthropic's prompt-engineering docs
//! specify XML tags as the preferred structural delimiter for LLM context windows.
//! (<https://docs.anthropic.com/en/docs/build-with-claude/prompt-engineering/use-xml-tags>)

use anyhow::{Context, Result};
use indexa_core::store::{Store, SummaryRecord};
use std::path::Path;

/// One node in the in-memory export tree.
pub struct ExportNode {
    pub record: SummaryRecord,
    pub children: Vec<ExportNode>,
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

/// Render the tree as XML (primary AI-context format).
pub fn render_xml(node: &ExportNode, generated_at: &str) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(&format!(
        r#"<index root="{}" generated_at="{}">"#,
        xml_attr(&node.record.path),
        xml_attr(generated_at),
    ));
    out.push('\n');
    render_xml_node(node, &mut out, 1);
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
    let mut out = String::with_capacity(4096);
    out.push_str(&format!("# Index: {}\n\n", node.record.path));
    render_md_node(node, &mut out, 2);
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
}
