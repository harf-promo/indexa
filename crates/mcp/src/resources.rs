//! MCP Resources: read-only index artifacts a client can list + attach without a tool
//! call. Each maps to an existing store/query read; any file/summary content is run
//! through `redact_secrets` (the same invariant as `export_pack`/`export`).
//!
//! URI scheme `indexa://…`:
//! - `indexa://overview`        — whole-project roll-up (markdown)
//! - `indexa://packs`           — Context Pack list (json)
//! - `indexa://pack/{name}`     — a Context Pack rendered as markdown (template, redacted)
//! - `indexa://summary/{path}`  — a file/dir summary (template, redacted)
//!
//! The methods are inherent + pure (no `RequestContext`) so they unit-test directly,
//! mirroring `read_file_inner`. The `ServerHandler` glue in `lib.rs` calls them.

use rmcp::model::{
    AnnotateAble, RawResource, RawResourceTemplate, ReadResourceResult, Resource, ResourceContents,
    ResourceTemplate,
};
use rmcp::ErrorData;

use indexa_query::redact::redact_secrets;

use crate::packs::export_pack_body;
use crate::{mcp_err, IndexaMcp};

const OVERVIEW_BUDGET: usize = 4000;

impl IndexaMcp {
    /// The static resource list (`resources/list`).
    pub(crate) fn list_resources_inner(&self) -> Vec<Resource> {
        vec![
            RawResource::new("indexa://overview", "Project overview")
                .with_description(
                    "Whole-project roll-up: directory summaries describing what this index covers.",
                )
                .with_mime_type("text/markdown")
                .no_annotation(),
            RawResource::new("indexa://packs", "Context Packs")
                .with_description("The list of named, cross-directory Context Packs (JSON).")
                .with_mime_type("application/json")
                .no_annotation(),
        ]
    }

    /// The parameterized resource templates (`resources/templates/list`).
    pub(crate) fn resource_templates_inner(&self) -> Vec<ResourceTemplate> {
        vec![
            RawResourceTemplate::new("indexa://pack/{name}", "Context Pack export")
                .with_description("A named Context Pack rendered as Markdown (secrets redacted).")
                .with_mime_type("text/markdown")
                .no_annotation(),
            RawResourceTemplate::new("indexa://summary/{path}", "File or directory summary")
                .with_description("The indexed summary of a file or directory (secrets redacted).")
                .with_mime_type("text/markdown")
                .no_annotation(),
        ]
    }

    /// Resolve a `indexa://…` URI to its contents (`resources/read`). Unknown URIs and
    /// missing data return an error result — never a panic.
    pub(crate) fn read_resource_inner(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let rest = uri
            .strip_prefix("indexa://")
            .ok_or_else(|| mcp_err(format!("unsupported resource URI: {uri}")))?;

        let text = if rest == "overview" {
            let store = self.store()?;
            let root = store.root_paths().ok().and_then(|r| r.into_iter().next());
            let overview =
                indexa_query::build_project_overview(&store, &[], root.as_deref(), OVERVIEW_BUDGET);
            if overview.trim().is_empty() {
                "No project overview yet. Run `indexa summarize` (or `indexa index`) first.".into()
            } else {
                overview
            }
        } else if rest == "packs" {
            let store = self.store()?;
            let packs = store.list_packs().map_err(mcp_err)?;
            let arr: Vec<_> = packs
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name,
                        "description": p.description,
                        "path_count": p.path_count,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_owned())
        } else if let Some(name) = rest.strip_prefix("pack/") {
            let name = percent_decode(name);
            let store = self.store()?;
            // Already redacted inside export_pack_body.
            export_pack_body(&store, &name, "md", None, false, None, None)?
        } else if let Some(path) = rest.strip_prefix("summary/") {
            let path = percent_decode(path);
            let store = self.store()?;
            // Confinement: you can only read summaries of indexed paths (Some ⇒ within roots).
            let rec = store
                .summary_by_path(&path)
                .map_err(mcp_err)?
                .ok_or_else(|| {
                    mcp_err(format!("no summary for {path}. Run `indexa summarize`."))
                })?;
            let (body, _n) = redact_secrets(&format!("# {}\n\n{}", path, rec.summary));
            body
        } else {
            return Err(mcp_err(format!("unknown resource: {uri}")));
        };

        let mime = if rest == "packs" {
            "application/json"
        } else {
            "text/markdown"
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )
        .with_mime_type(mime)]))
    }
}

/// Minimal percent-decode for the `{name}`/`{path}` template segments (handles `%XX`;
/// leaves anything malformed as-is). Avoids a urlencoding dependency.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
