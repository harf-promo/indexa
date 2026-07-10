//! Context Pack tools: `list_packs`, `get_pack`, `export_pack`, `create_pack`,
//! `add_pack_paths`, `remove_pack_paths`, `delete_pack`, and `search_pack`.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::{config::HybridMode, store::Store};

use crate::{mcp_err, ok_text, record_usage, IndexaMcp};

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
    /// Relational slice: keep only files modified within this window (e.g. `7d`, `12h`, `90m`,
    /// `3600s`). Combine with `category` to intersect. Omit for no recency filter.
    #[serde(default)]
    pub changed_since: Option<String>,
    /// Relational slice: keep only files in this classification category (e.g. `code`, `docs`).
    /// Combine with `changed_since` to intersect. Omit for no category filter.
    #[serde(default)]
    pub category: Option<String>,
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
                       hierarchical summary tree. Optionally slice with `changed_since` \
                       (e.g. '7d') and/or `category` (e.g. 'code'). Ideal for giving an AI tool \
                       focused context on a specific topic (e.g. 'Auth', 'Tax 2025', 'Client X')."
    )]
    pub(crate) async fn export_pack(
        &self,
        params: Parameters<ExportPackParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let ExportPackParams {
            name,
            format,
            depth,
            signatures,
            changed_since,
            category,
        } = params.0;
        let mut store = self.store()?;
        let buf = export_pack_body(
            &store,
            &name,
            format.as_deref().unwrap_or("xml"),
            depth,
            signatures.unwrap_or(false),
            changed_since.as_deref(),
            category.as_deref(),
        )?;
        // Savings telemetry: the export (buf) vs. the counterfactual of reading every pack file
        // whole. Best-effort — a failed lookup just records a zero counterfactual.
        let counterfactual = store
            .pack_by_name(&name)
            .ok()
            .flatten()
            .map(|p| {
                let paths = store.pack_paths(&p.id).unwrap_or_default();
                let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
                store.counterfactual_bytes_for_paths(&refs).unwrap_or(0)
            })
            .unwrap_or(0);
        record_usage(&mut store, "export_pack", buf.len(), counterfactual);
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
                       named Context Pack. Returns matching chunks with path, heading, and snippet; \
                       each hit shows `#N` (the chunk seq) to pass to `get_chunk_context`. \
                       Ideal for querying focused topic bundles (e.g. 'Auth', 'Tax 2025')."
    )]
    pub(crate) async fn search_pack(
        &self,
        params: Parameters<SearchPackParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let SearchPackParams { name, query, limit } = params.0;
        let limit = limit.unwrap_or(20).min(100);

        let embedding = self.embedder.embed(&query).await.ok();
        let mut store = self.store()?;

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
                format!("{}{} #{}\n  {}", h.entry_path, heading, h.seq, snippet)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = format!("{} result(s) in pack \"{name}\":\n\n{body}", hits.len());
        // Savings telemetry: the snippets served vs. reading the matched files whole (same basis
        // as the `search` tool). Best-effort; a lookup failure records a zero counterfactual.
        let paths: Vec<&str> = hits.iter().map(|h| h.entry_path.as_str()).collect();
        let counterfactual = store.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
        record_usage(&mut store, "search_pack", out.len(), counterfactual);
        Ok(ok_text(out))
    }
}

/// Render a Context Pack to a single string in `format` (`xml` | `md` | `json`), with
/// `redact_secrets` applied — the shared body for the `export_pack` tool, the
/// `indexa://pack/{name}` resource, and the `pack-context` prompt (one redaction site).
pub(crate) fn export_pack_body(
    store: &Store,
    name: &str,
    format: &str,
    depth: Option<usize>,
    signatures: bool,
    changed_since: Option<&str>,
    category: Option<&str>,
) -> Result<String, ErrorData> {
    use indexa_query::{
        build_export_filter, build_tree, prune_tree, redact::redact_secrets, render_json,
        render_markdown, render_signatures, render_xml,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now = now_secs.to_string();

    let pack = store
        .pack_by_name(name)
        .map_err(mcp_err)?
        .ok_or_else(|| mcp_err(format!("no pack named \"{name}\"")))?;
    let paths = store.pack_paths(&pack.id).map_err(mcp_err)?;
    if paths.is_empty() {
        return Err(mcp_err(format!(
            "pack \"{name}\" is empty — add paths first with: indexa pack add \"{name}\" <paths…>"
        )));
    }

    // Relational slice (v0.58/v0.60): same filters as CLI `pack export` and web `/api/packs/:name/export`,
    // shared via build_export_filter. `None` ⇒ export the whole pack (byte-identical to before).
    let allow = build_export_filter(store, changed_since, category, now_secs).map_err(mcp_err)?;

    let is_xml = format != "md" && format != "markdown" && format != "json";
    let mut buf = String::new();
    if is_xml {
        buf.push_str("<context pack=\"");
        buf.push_str(&indexa_core::text::xml_escape_attr(name));
        buf.push_str("\" generated=\"");
        buf.push_str(&now);
        buf.push_str("\">\n");
    }

    let mut exported = 0usize;
    for root_path in &paths {
        if signatures {
            let mut chunks = store.code_chunks_under(root_path, 0).map_err(mcp_err)?;
            if let Some(a) = &allow {
                chunks.retain(|c| a.contains(&c.entry_path));
            }
            if chunks.is_empty() {
                continue;
            }
            buf.push_str(&render_signatures(&chunks, format, true));
            buf.push('\n');
            exported += 1;
            continue;
        }
        let tree = build_tree(store, root_path, depth).map_err(mcp_err)?;
        let Some(tree) = tree else { continue };
        // Apply the relational slice; skip a path that matched nothing.
        let tree = match &allow {
            Some(a) => match prune_tree(tree, a) {
                Some(t) => t,
                None => continue,
            },
            None => tree,
        };
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
        let msg = if allow.is_some() {
            format!(
                "nothing in pack \"{name}\" matched the slice (changed_since / category) \
                 — widen it or drop the filter"
            )
        } else {
            let hint = if signatures {
                "have indexed code yet — run `indexa deep <path>` first"
            } else {
                "have summaries yet — run `indexa summarize <path>` or `indexa index <path>` first"
            };
            format!("no paths in pack \"{name}\" {hint}")
        };
        return Err(mcp_err(msg));
    }

    // Never hand a model a secret that slipped into the indexed content.
    let (buf, _redacted) = redact_secrets(&buf);
    Ok(buf)
}
