//! Context Pack tools: `list_packs`, `get_pack`, `export_pack`, `create_pack`,
//! `add_pack_paths`, `remove_pack_paths`, `delete_pack`, and `search_pack`.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::{config::HybridMode, store::Store};

use crate::{mcp_err, ok_text, xml_escape_mcp, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetPackParams {
    /// Name of the Context Pack to retrieve (case-insensitive).
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExportPackParams {
    /// Name of the Context Pack to export (case-insensitive).
    pub name: String,
    /// Output format: `xml` (default), `md`, or `json`.
    #[serde(default)]
    pub format: Option<String>,
    /// Maximum tree depth per path (0 = top summary only). Omit for full depth.
    #[serde(default)]
    pub depth: Option<usize>,
    /// Emit a code-skeleton view (symbol signatures, bodies elided) instead of prose summaries —
    /// far fewer tokens for handing code structure to a model. Reads indexed chunks.
    #[serde(default)]
    pub signatures: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreatePackMcpParams {
    /// Pack name (must be unique).
    pub name: String,
    /// Optional short description.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PackPathsParams {
    /// Pack name (case-insensitive).
    pub name: String,
    /// List of absolute file or directory paths.
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeletePackMcpParams {
    /// Pack name to delete (case-insensitive). Does not remove indexed files.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchPackParams {
    /// Pack name to search within (case-insensitive).
    pub name: String,
    /// Keyword / semantic query.
    pub query: String,
    /// Max results (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[tool_router(router = router_packs, vis = "pub(crate)")]
impl IndexaMcp {
    /// List all Context Packs with their path counts.
    #[tool(
        description = "List all Context Packs — named, cross-directory context bundles. \
                       Returns each pack's name, description, and path count. \
                       Use `get_pack` to see the paths inside a specific pack, \
                       or `export_pack` to render its content for an AI tool."
    )]
    pub(crate) async fn list_packs(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let packs = store.list_packs().map_err(mcp_err)?;
        if packs.is_empty() {
            return Ok(ok_text(
                "No Context Packs yet. Create one with: indexa pack create \"<name>\"",
            ));
        }
        let lines: Vec<String> = packs
            .iter()
            .map(|p| {
                let desc = p
                    .description
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default();
                format!("{}{} ({} paths)", p.name, desc, p.path_count)
            })
            .collect();
        Ok(ok_text(format!(
            "{} pack(s):\n\n{}",
            packs.len(),
            lines.join("\n")
        )))
    }

    /// Show the paths inside a named Context Pack.
    #[tool(
        description = "Show the file/directory paths contained in a named Context Pack. \
                       Use `export_pack` to render the full summarised content."
    )]
    pub(crate) async fn get_pack(
        &self,
        params: Parameters<GetPackParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let GetPackParams { name } = params.0;
        let store = self.store()?;
        let pack = store
            .pack_by_name(&name)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
        let paths = store.pack_paths(&pack.id).map_err(mcp_err)?;
        if paths.is_empty() {
            return Ok(ok_text(format!(
                "Pack \"{name}\" is empty. Add paths with: indexa pack add \"{name}\" <paths…>"
            )));
        }
        Ok(ok_text(format!(
            "Pack \"{name}\" ({} paths):\n\n{}",
            paths.len(),
            paths.join("\n")
        )))
    }

    /// Export a Context Pack as XML, Markdown, or JSON — ready to paste into any AI tool.
    #[tool(
        description = "Export a Context Pack as a self-contained context file (XML by default, \
                       also Markdown or JSON). Each path in the pack is rendered with its \
                       hierarchical summary tree. Ideal for giving an AI tool focused context \
                       on a specific topic (e.g. 'Auth', 'Tax 2025', 'Client X')."
    )]
    pub(crate) async fn export_pack(
        &self,
        params: Parameters<ExportPackParams>,
    ) -> Result<CallToolResult, ErrorData> {
        use indexa_query::{
            build_tree, redact::redact_secrets, render_json, render_markdown, render_signatures,
            render_xml,
        };
        use std::time::{SystemTime, UNIX_EPOCH};

        let ExportPackParams {
            name,
            format,
            depth,
            signatures,
        } = params.0;
        let signatures = signatures.unwrap_or(false);
        let format = format.as_deref().unwrap_or("xml");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_owned());

        let store = self.store()?;
        let pack = store
            .pack_by_name(&name)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
        let paths = store.pack_paths(&pack.id).map_err(mcp_err)?;
        if paths.is_empty() {
            return Err(mcp_err(format!(
                "pack \"{name}\" is empty — add paths first with: \
                 indexa pack add \"{name}\" <paths…>"
            )));
        }

        let is_xml = format != "md" && format != "markdown" && format != "json";
        let mut buf = String::new();
        if is_xml {
            buf.push_str("<context pack=\"");
            buf.push_str(&xml_escape_mcp(&name));
            buf.push_str("\" generated=\"");
            buf.push_str(&now);
            buf.push_str("\">\n");
        }

        let mut exported = 0usize;
        for root_path in &paths {
            if signatures {
                let chunks = store.code_chunks_under(root_path, 0).map_err(mcp_err)?;
                if chunks.is_empty() {
                    continue;
                }
                buf.push_str(&render_signatures(&chunks, format, true));
                buf.push('\n');
                exported += 1;
                continue;
            }
            let tree = build_tree(&store, root_path, depth).map_err(mcp_err)?;
            let Some(tree) = tree else { continue };
            let rendered = match format {
                "md" | "markdown" => render_markdown(&tree),
                "json" => render_json(&tree),
                _ => render_xml(&tree, &now),
            };
            buf.push_str(&rendered);
            buf.push('\n');
            exported += 1;
        }
        if is_xml {
            buf.push_str("</context>\n");
        }

        if exported == 0 {
            let hint = if signatures {
                "have indexed code yet — run `indexa deep <path>` first"
            } else {
                "have summaries yet — run `indexa summarize <path>` or `indexa index <path>` first"
            };
            return Err(mcp_err(format!("no paths in pack \"{name}\" {hint}")));
        }

        // Never hand a model a secret that slipped into the indexed content.
        let (buf, _redacted) = redact_secrets(&buf);
        Ok(ok_text(buf))
    }

    // ── Context Pack mutations ─────────────────────────────────────────────────

    /// Create a new (empty) Context Pack.
    #[tool(
        description = "Create a new named Context Pack. Packs are cross-directory context \
                       bundles you can populate with `add_pack_paths` and export for any AI tool."
    )]
    pub(crate) async fn create_pack(
        &self,
        params: Parameters<CreatePackMcpParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let CreatePackMcpParams { name, description } = params.0;
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        let id = store
            .create_pack(&name, description.as_deref())
            .map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Created pack \"{name}\" (id: {id}). \
             Add paths with `add_pack_paths`."
        )))
    }

    /// Add paths to an existing Context Pack.
    #[tool(
        description = "Add one or more file or directory paths to a named Context Pack. \
                       Duplicate paths are silently ignored (idempotent)."
    )]
    pub(crate) async fn add_pack_paths(
        &self,
        params: Parameters<PackPathsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let PackPathsParams { name, paths } = params.0;
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        let pack = store
            .pack_by_name(&name)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
        let count = paths.len();
        store.add_pack_paths(&pack.id, &paths).map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Added {count} path{} to pack \"{name}\".",
            if count == 1 { "" } else { "s" }
        )))
    }

    /// Remove paths from a Context Pack.
    #[tool(description = "Remove specific paths from a named Context Pack. \
                       Non-existent paths are silently ignored. Indexed files are not deleted.")]
    pub(crate) async fn remove_pack_paths(
        &self,
        params: Parameters<PackPathsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let PackPathsParams { name, paths } = params.0;
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        let pack = store
            .pack_by_name(&name)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
        let count = paths.len();
        store.remove_pack_paths(&pack.id, &paths).map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Removed {count} path{} from pack \"{name}\".",
            if count == 1 { "" } else { "s" }
        )))
    }

    /// Delete a Context Pack (indexed files are untouched).
    #[tool(description = "Delete a Context Pack and all its path associations. \
                       Does not remove indexed files from the index.")]
    pub(crate) async fn delete_pack(
        &self,
        params: Parameters<DeletePackMcpParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let DeletePackMcpParams { name } = params.0;
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        let pack = store
            .pack_by_name(&name)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
        store.delete_pack(&pack.id).map_err(mcp_err)?;
        Ok(ok_text(format!("Deleted pack \"{name}\".")))
    }

    /// Search indexed content scoped to the paths in a Context Pack.
    #[tool(
        description = "Search chunk content restricted to the file/directory paths inside a \
                       named Context Pack. Returns matching chunks with path, heading, and snippet. \
                       Ideal for querying focused topic bundles (e.g. 'Auth', 'Tax 2025')."
    )]
    pub(crate) async fn search_pack(
        &self,
        params: Parameters<SearchPackParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let SearchPackParams { name, query, limit } = params.0;
        let limit = limit.unwrap_or(20).min(100);

        let embedding = self.embedder.embed(&query).await.ok();
        let store = self.store()?;

        let pack = store
            .pack_by_name(&name)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
        let paths = store.pack_paths(&pack.id).map_err(mcp_err)?;
        if paths.is_empty() {
            return Ok(ok_text(format!("Pack \"{name}\" is empty.")));
        }

        // Search once per pack path prefix, then merge by RRF score.
        let per_scope = (limit * 2).max(10);
        let mut all_hits: Vec<indexa_core::store::SearchHit> = Vec::new();
        for root in &paths {
            let scope = root.as_str();
            if let Ok(mut hits) = store.hybrid_search(
                &query,
                embedding.as_deref(),
                &HybridMode::Rrf,
                Some(scope),
                per_scope,
                60.0,
            ) {
                all_hits.append(&mut hits);
            }
        }

        // Deduplicate by (entry_path, seq) keeping highest rrf_score, then take top limit.
        all_hits.sort_by(|a, b| {
            b.rrf_score
                .partial_cmp(&a.rrf_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut seen = std::collections::HashSet::new();
        let hits: Vec<_> = all_hits
            .into_iter()
            .filter(|h| seen.insert(format!("{}:{}", h.entry_path, h.seq)))
            .take(limit)
            .collect();

        if hits.is_empty() {
            return Ok(ok_text(format!(
                "No results for '{query}' within pack \"{name}\"."
            )));
        }
        let body = hits
            .iter()
            .map(|h| {
                let heading = if h.heading.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", h.heading)
                };
                let snippet: String = h.text.chars().take(120).collect();
                format!("{}{}\n  {}", h.entry_path, heading, snippet)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(ok_text(format!(
            "{} result(s) in pack \"{name}\":\n\n{body}",
            hits.len()
        )))
    }
}
