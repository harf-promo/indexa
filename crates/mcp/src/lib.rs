//! MCP (Model Context Protocol) server exposing the Indexa index to AI agents.
//!
//! Started via `indexa mcp`, it speaks JSON-RPC over **stdio** so clients like
//! Claude Desktop and Cursor can browse the local index live as tool calls. It
//! reuses the existing `Store` and `query` functions directly — no HTTP layer.
//!
//! **stdout is the protocol channel** — all logging must go to stderr.
//!
//! Tools (28): `search`, `browse_tree`, `get_summary` (tier l0/l1/l2 — progressive
//! disclosure), `read_file`, `ask`, `dependencies` (a file's imports, defined symbols,
//! and calls), `who_imports` (reverse code-graph lookup), `who_calls` (D2 — reverse
//! call lookup), `blast_radius` (D2 — 1-hop call blast radius), `get_stats`,
//! `list_packs`, `get_pack`, `export_pack`, `create_pack`, `add_pack_paths`,
//! `remove_pack_paths`, `delete_pack` (Context Packs),
//! `list_classifications`, `confirm_classification`, `ignore_classification`
//! (Smart classification), `trigger_index` (indexing trigger),
//! `search_pack` (scoped content search within a pack),
//! `list_weights`, `set_weight`, `delete_weight` (v0.8 Importance weighting),
//! `insights_duplicates`, `insights_stale`, `insights_diff` (v0.10 Insights).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt,
};
use serde::Deserialize;

use indexa_core::{
    config::{Config, HybridMode},
    store::Store,
};
use indexa_embed::Embedder;
use indexa_llm::Generator;
use indexa_query::QaConfig;

/// Max bytes returned by `read_file` (L2 raw content).
const READ_FILE_CAP: usize = 40 * 1024;

/// The Indexa MCP server handler. Holds only `Send + Sync` state. Each tool opens
/// its own short-lived `Store` connection (a rusqlite `Connection` is `Send` but
/// not `Sync`, so it can't be shared across the async tool futures) — mirroring
/// how the CLI commands each open the store. Connection open is cheap.
#[derive(Clone)]
pub struct IndexaMcp {
    db_path: Arc<PathBuf>,
    embedder: Arc<dyn Embedder + Send + Sync>,
    llm: Arc<dyn Generator + Send + Sync>,
    config: Arc<Config>,
}

// ── Tool parameter structs (Deserialize + JsonSchema for the tool input schema) ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Keyword query to search across file **content** (chunk text + headings, BM25 + vector).
    pub query: String,
    /// Max results to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Scope results to this path prefix (optional).
    #[serde(default)]
    pub scope: Option<String>,
    /// Retrieval mode: `rrf` (default — hybrid BM25 + vector), `sparse` (BM25 only, no
    /// embedder needed), `dense` (vector only, requires embeddings).
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BrowseParams {
    /// Absolute directory path to list children of. Empty for indexed roots.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetSummaryParams {
    /// Absolute path of the file or directory.
    pub path: String,
    /// Detail tier: `l0` (one-line abstract), `l1` (full summary, default), or
    /// `l2` (raw file content). Survey on l0, drill to l1/l2 on demand.
    #[serde(default)]
    pub tier: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    /// Absolute path of the file to read (raw content, truncated to ~40 KB).
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskParams {
    /// Natural-language question answered against the indexed context.
    pub question: String,
    /// Restrict retrieval to this path prefix (e.g. `~/code/myproject`). Omit for whole index.
    #[serde(default)]
    pub scope: Option<String>,
    /// Retrieval mode: `rrf` (default — hybrid BM25 + vector), `sparse` (BM25 only),
    /// `dense` (vector only).
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependenciesParams {
    /// Absolute path of an indexed code file.
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WhoImportsParams {
    /// Module/import path to find importers of, exactly as written in source
    /// (e.g. `std::fs`, `os`, `./util`).
    pub module: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WhoCallsParams {
    /// Bare function or method name to find callers of (e.g. `parse`, `render`, `connect`).
    pub symbol: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlastRadiusParams {
    /// Bare function or method name whose blast radius to compute.
    pub symbol: String,
}

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListClassificationsParams {
    /// Source filter: `auto`, `user`, or `ignored`. Omit to return all.
    #[serde(default)]
    pub source: Option<String>,
    /// Max rows to return (default 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfirmClassificationParams {
    /// Absolute path of the file or directory.
    pub path: String,
    /// Category to assign (e.g. `work`, `personal`, `code`, `media`).
    pub category: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IgnoreClassificationParams {
    /// Absolute path of the file or directory to suppress from suggestions.
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TriggerIndexParams {
    /// Absolute path to scan, deep-index, and summarize.
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetWeightParams {
    /// Target kind: `file`, `dir`, or `category`.
    pub target_kind: String,
    /// Absolute path or category name.
    pub target: String,
    /// Weight value: 0.0 = silence, 1.0 = neutral, >1.0 = boost.
    pub weight: f32,
    /// Optional human-readable reason for this weight.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteWeightParams {
    /// Target kind: `file`, `dir`, or `category`.
    pub target_kind: String,
    /// Absolute path or category name.
    pub target: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InsightsDuplicatesParams {
    /// Similarity threshold for near-duplicate detection (default 0.95).
    #[serde(default)]
    pub threshold: Option<f32>,
    /// Find exact duplicates only (by content hash, no embedder required).
    #[serde(default)]
    pub exact: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InsightsDaysParams {
    /// Number of days for the look-back window.
    #[serde(default)]
    pub days: Option<i64>,
}

fn mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn ok_text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s.into())])
}

#[tool_router]
impl IndexaMcp {
    pub fn new(
        db_path: PathBuf,
        embedder: Arc<dyn Embedder + Send + Sync>,
        llm: Arc<dyn Generator + Send + Sync>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            db_path: Arc::new(db_path),
            embedder,
            llm,
            config,
        }
    }

    /// Open a fresh read connection to the index (cheap; avoids sharing a
    /// non-`Sync` rusqlite handle across the async tool futures).
    fn store(&self) -> Result<Store, ErrorData> {
        Store::open(&self.db_path).map_err(mcp_err)
    }

    /// Hybrid keyword + semantic search over indexed **chunk content** (BM25 + vector RRF).
    /// Returns matching chunks with their file path, heading, and a text snippet.
    /// Use `scope` to restrict to a subtree. For path-name browsing, prefer `browse_tree`.
    #[tool(
        description = "Search indexed chunk content by keyword (BM25 + vector hybrid). Returns matching chunks with path, heading, and snippet — richer than path-name search. Optionally scope to a path prefix."
    )]
    async fn search(&self, params: Parameters<SearchParams>) -> Result<CallToolResult, ErrorData> {
        let SearchParams {
            query,
            limit,
            scope,
            mode,
        } = params.0;
        let limit = limit.unwrap_or(20).min(100);
        let scope = scope.as_deref().filter(|s| !s.is_empty());
        let mode = parse_hybrid_mode(mode.as_deref());

        // Try to embed the query for the dense arm; fall back to sparse if the embedder is
        // unavailable or the index has no embeddings.
        let embedding = if matches!(mode, HybridMode::Sparse) {
            None
        } else {
            self.embedder.embed(&query).await.ok()
        };

        let store = self.store()?;
        let hits = store
            .hybrid_search(&query, embedding.as_deref(), &mode, scope, limit, 60.0)
            .map_err(mcp_err)?;

        if hits.is_empty() {
            return Ok(ok_text(format!("No results for '{query}'.")));
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
        Ok(ok_text(format!("{} result(s):\n\n{body}", hits.len())))
    }

    /// List a code file's dependencies from the code graph (imports + defined symbols).
    #[tool(
        description = "List a code file's dependencies from the code graph: the modules/paths it imports and the symbols (functions, types, classes) it defines. Requires an absolute path to a file indexed with `indexa deep`."
    )]
    async fn dependencies(
        &self,
        params: Parameters<DependenciesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let edges = store.edges_from(&params.0.path).map_err(mcp_err)?;
        if edges.is_empty() {
            return Ok(ok_text(format!(
                "No code-graph edges for {}. Run `indexa deep` on a code file first.",
                params.0.path
            )));
        }
        let line = |prefix: &str, items: Vec<&str>| {
            items
                .iter()
                .map(|s| format!("  {prefix} {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let imports: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "imports")
            .map(|e| e.to_ref.as_str())
            .collect();
        let defines: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "defines")
            .map(|e| e.to_ref.as_str())
            .collect();
        let calls: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| e.to_ref.as_str())
            .collect();
        let mut out = String::new();
        if !imports.is_empty() {
            out.push_str(&format!(
                "Imports ({}):\n{}\n",
                imports.len(),
                line("→", imports)
            ));
        }
        if !defines.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "Defines ({}):\n{}",
                defines.len(),
                line("•", defines)
            ));
        }
        if !calls.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("\nCalls ({}):\n{}", calls.len(), line("↪", calls)));
        }
        Ok(ok_text(out))
    }

    /// Reverse dependency: which indexed files import a given module/path.
    #[tool(
        description = "Reverse dependency lookup over the code graph: which indexed files import a given module/path (as written in source). Use to find a module's dependents."
    )]
    async fn who_imports(
        &self,
        params: Parameters<WhoImportsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let files = store
            .edges_to("imports", &params.0.module)
            .map_err(mcp_err)?;
        if files.is_empty() {
            return Ok(ok_text(format!(
                "No indexed file imports '{}'.",
                params.0.module
            )));
        }
        // Cap the listing so a ubiquitous module (imported everywhere) can't flood the
        // client's context; report the true total and how many are shown.
        const MAX_SHOWN: usize = 100;
        let total = files.len();
        let body = files
            .iter()
            .take(MAX_SHOWN)
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let header = if total > MAX_SHOWN {
            format!(
                "{total} file(s) import '{}' (showing first {MAX_SHOWN}):",
                params.0.module
            )
        } else {
            format!("{total} file(s) import '{}':", params.0.module)
        };
        Ok(ok_text(format!("{header}\n{body}")))
    }

    /// D2 — which files call a given function or method name.
    #[tool(
        description = "D2 code-graph: which indexed files contain a call to the given function or method name (bare, unqualified — e.g. `parse`, `render`, `connect`). Requires `indexa deep` to have been run on source files. Returns up to 100 results."
    )]
    async fn who_calls(
        &self,
        params: Parameters<WhoCallsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let files = store.who_calls(&params.0.symbol, 100).map_err(mcp_err)?;
        if files.is_empty() {
            return Ok(ok_text(format!(
                "No indexed file calls '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = files.len();
        let body = files
            .iter()
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(format!(
            "{total} file(s) call '{}':\n{body}",
            params.0.symbol
        )))
    }

    /// D2 — 1-hop blast radius for a symbol: direct callers and transitive callers.
    #[tool(
        description = "D2 code-graph: compute the blast radius of changing a function or method — returns the direct callers plus files that call any symbol defined in those callers (1-hop transitive). Use to answer 'what breaks if I change X?'. Returns up to 200 results."
    )]
    async fn blast_radius(
        &self,
        params: Parameters<BlastRadiusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let files = store.blast_radius(&params.0.symbol, 200).map_err(mcp_err)?;
        if files.is_empty() {
            return Ok(ok_text(format!(
                "No blast radius found for '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = files.len();
        let body = files
            .iter()
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(format!(
            "Blast radius of '{}' ({total} file(s)):\n{body}",
            params.0.symbol
        )))
    }

    /// List the direct children (with summary state) of a directory.
    #[tool(
        description = "List the direct children of a directory in the index, with each child's kind and file/chunk counts. Empty path lists indexed roots."
    )]
    async fn browse_tree(
        &self,
        params: Parameters<BrowseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let nodes = store.tree_level(&params.0.path).map_err(mcp_err)?;
        if nodes.is_empty() {
            return Ok(ok_text("No children (empty or not an indexed directory)."));
        }
        let body = nodes
            .iter()
            .map(|n| {
                let icon = if n.kind == "dir" { "📁" } else { "📄" };
                let counts = if n.kind == "dir" {
                    format!(" ({} files, {} chunks)", n.file_count, n.chunk_count)
                } else {
                    String::new()
                };
                format!("{icon} {}{counts}", n.path)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(body))
    }

    /// Get a node's summary at the requested tier (l0 abstract / l1 full / l2 raw).
    #[tool(
        description = "Get a file or directory's summary. tier='l0' returns the one-line abstract (cheap, for scanning), 'l1' the full summary (default), 'l2' the raw file content. For directories, also lists child abstracts. The progressive-disclosure entry point."
    )]
    async fn get_summary(
        &self,
        params: Parameters<GetSummaryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let GetSummaryParams { path, tier } = params.0;
        let tier = tier.as_deref().unwrap_or("l1").to_lowercase();

        if tier == "l2" {
            return self.read_file_inner(&path);
        }

        let store = self.store()?;
        let rec = match store.summary_by_path(&path).map_err(mcp_err)? {
            Some(r) => r,
            None => {
                return Ok(ok_text(format!(
                    "No summary for {path}. Run `indexa summarize` first."
                )))
            }
        };
        let mut out = String::new();
        if tier == "l0" {
            out.push_str(rec.summary_l0.as_deref().unwrap_or(&rec.summary));
        } else {
            // l1 (default)
            out.push_str(&format!("# {}\n\n{}", path, rec.summary));
            if rec.kind == "dir" {
                let children = store.children_summaries(&path).map_err(mcp_err)?;
                if !children.is_empty() {
                    out.push_str("\n\n## Contents\n");
                    for c in children.iter().take(50) {
                        let icon = if c.kind == "dir" { "📁" } else { "📄" };
                        let name = std::path::Path::new(&c.path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| c.path.clone());
                        let abstract_ = c.summary_l0.as_deref().unwrap_or(&c.summary);
                        out.push_str(&format!("- {icon} {name}: {abstract_}\n"));
                    }
                }
            }
        }
        Ok(ok_text(out))
    }

    /// Read raw file content (L2).
    #[tool(description = "Read the raw text content of an indexed file (truncated to ~40 KB).")]
    async fn read_file(
        &self,
        params: Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.read_file_inner(&params.0.path)
    }

    /// Answer a natural-language question against the index (grounded RAG).
    #[tool(
        description = "Answer a natural-language question using the indexed context (hybrid retrieval + local LLM synthesis). Returns an answer with source paths."
    )]
    async fn ask(&self, params: Parameters<AskParams>) -> Result<CallToolResult, ErrorData> {
        let AskParams {
            question,
            scope,
            mode,
        } = params.0;
        let cfg = QaConfig {
            top_k: self.config.retrieval.top_k,
            mode: mode
                .as_deref()
                .map(|m| parse_hybrid_mode(Some(m)))
                .unwrap_or_else(|| self.config.retrieval.hybrid.clone()),
            scope: scope.filter(|s| !s.is_empty()),
            context_budget: self.config.retrieval.context_budget,
            rrf_k: self.config.retrieval.rrf_k as f32,
            summary_weight: self.config.retrieval.summary_weight,
            summary_depth_alpha: self.config.retrieval.summary_depth_alpha,
            rerank: self.config.retrieval.rerank,
            use_weights: self.config.retrieval.use_weights,
        };

        // Single shared, Send-safe pipeline (embed → scoped retrieve → optional
        // rerank → synthesize). `answer` opens its own short-lived read connection
        // from `db_path`; the empty-hit short-circuit lives inside it.
        let answer = indexa_query::answer(
            &self.db_path,
            self.embedder.as_ref(),
            self.llm.as_ref(),
            &question,
            &cfg,
        )
        .await
        .map_err(mcp_err)?;

        let mut out = answer.answer;
        if !answer.sources.is_empty() {
            out.push_str("\n\nSources:\n");
            for s in &answer.sources {
                out.push_str(&format!("- {}\n", s.path));
            }
        }
        Ok(ok_text(out))
    }

    /// List all Context Packs with their path counts.
    #[tool(
        description = "List all Context Packs — named, cross-directory context bundles. \
                       Returns each pack's name, description, and path count. \
                       Use `get_pack` to see the paths inside a specific pack, \
                       or `export_pack` to render its content for an AI tool."
    )]
    async fn list_packs(&self) -> Result<CallToolResult, ErrorData> {
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
    async fn get_pack(
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
    async fn export_pack(
        &self,
        params: Parameters<ExportPackParams>,
    ) -> Result<CallToolResult, ErrorData> {
        use indexa_query::{build_tree, render_json, render_markdown, render_xml};
        use std::time::{SystemTime, UNIX_EPOCH};

        let ExportPackParams {
            name,
            format,
            depth,
        } = params.0;
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
            return Err(mcp_err(format!(
                "no paths in pack \"{name}\" have summaries yet \
                 — run `indexa summarize <path>` or `indexa index <path>` first"
            )));
        }

        Ok(ok_text(buf))
    }

    /// Trigger a full scan → deep-index → summarize pipeline on a path.
    #[tool(
        description = "Start an `indexa index <path>` run: scan files, compute embeddings, \
                       and generate summaries. Runs as a background subprocess and returns \
                       when indexing is complete. Use before asking questions about new or \
                       changed files."
    )]
    async fn trigger_index(
        &self,
        params: Parameters<TriggerIndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = params.0.path;
        // Spawn `indexa index <path>` as a subprocess. The indexa binary is on PATH
        // (the same binary serving this MCP session). Both processes open the same DB
        // via WAL + 5s busy_timeout, which handles contention safely.
        let output = tokio::process::Command::new("indexa")
            .args(["index", &path])
            .output()
            .await
            .map_err(|e| mcp_err(format!("failed to spawn `indexa index`: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            let summary = if stdout.trim().is_empty() {
                format!("indexa index {path} completed successfully.")
            } else {
                stdout.trim().to_owned()
            };
            Ok(ok_text(summary))
        } else {
            Err(mcp_err(format!(
                "indexa index {path} failed (exit {:?}):\n{}",
                output.status.code(),
                if stderr.trim().is_empty() {
                    &stdout
                } else {
                    &stderr
                }
                .trim()
            )))
        }
    }

    // ── Context Pack mutations ─────────────────────────────────────────────────

    /// Create a new (empty) Context Pack.
    #[tool(
        description = "Create a new named Context Pack. Packs are cross-directory context \
                       bundles you can populate with `add_pack_paths` and export for any AI tool."
    )]
    async fn create_pack(
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
    async fn add_pack_paths(
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
    async fn remove_pack_paths(
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
    async fn delete_pack(
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
    async fn search_pack(
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

    // ── Smart classification ───────────────────────────────────────────────────

    /// List auto-suggested or user-confirmed classifications.
    #[tool(
        description = "List Smart classification records — auto-detected or user-confirmed \
                       category labels for files and directories. Filter by source: `auto`, \
                       `user`, or `ignored`."
    )]
    async fn list_classifications(
        &self,
        params: Parameters<ListClassificationsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let ListClassificationsParams { source, limit } = params.0;
        let limit = limit.unwrap_or(200);
        let store = self.store()?;
        let recs = store
            .list_classifications(source.as_deref(), limit)
            .map_err(mcp_err)?;
        if recs.is_empty() {
            return Ok(ok_text("No classifications found."));
        }
        let lines: Vec<String> = recs
            .iter()
            .map(|r| {
                format!(
                    "[{}] {} → {} (confidence: {:.0}%)",
                    r.source,
                    r.path,
                    r.category,
                    r.confidence * 100.0
                )
            })
            .collect();
        Ok(ok_text(format!(
            "{} classification(s):\n\n{}",
            recs.len(),
            lines.join("\n")
        )))
    }

    /// Confirm (or correct) a Smart classification — sets source to 'user'.
    #[tool(
        description = "Confirm or correct a Smart classification. Sets the source to 'user' \
                       so it persists across re-classify runs. \
                       A later auto pass will not overwrite a user decision."
    )]
    async fn confirm_classification(
        &self,
        params: Parameters<ConfirmClassificationParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let ConfirmClassificationParams { path, category } = params.0;
        let store = self.store()?;
        store
            .confirm_classification(&path, &category)
            .map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Classification for \"{path}\" confirmed as \"{category}\"."
        )))
    }

    /// Ignore a classification — sets a sticky tombstone so it is not re-proposed.
    #[tool(
        description = "Suppress a Smart classification suggestion permanently. \
                       Sets source='ignored' — a tombstone that prevents the path from \
                       being re-proposed on the next classify run."
    )]
    async fn ignore_classification(
        &self,
        params: Parameters<IgnoreClassificationParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let IgnoreClassificationParams { path } = params.0;
        let store = self.store()?;
        store.ignore_classification(&path).map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Classification for \"{path}\" suppressed."
        )))
    }

    // ── Importance weights (v0.8) ──────────────────────────────────────────────

    /// List all importance weights stored in the index.
    #[tool(
        description = "List all importance weights (v0.8). Each weight boosts or suppresses \
                       a file, directory, or classification category in search results. \
                       weight > 1.0 = boost; weight < 1.0 = suppress; 1.0 = neutral."
    )]
    async fn list_weights(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let weights = store.list_weights(None).map_err(mcp_err)?;
        if weights.is_empty() {
            return Ok(ok_text(
                "No importance weights set. Use `set_weight` to add one.",
            ));
        }
        let lines: Vec<String> = weights
            .iter()
            .map(|w| {
                format!(
                    "[{}] {} → {} (weight: {:.2}, source: {})",
                    w.target_kind, w.target, w.weight, w.weight, w.source
                )
            })
            .collect();
        Ok(ok_text(format!(
            "{} weight(s):\n\n{}",
            weights.len(),
            lines.join("\n")
        )))
    }

    /// Set an importance weight for a file, directory, or category.
    #[tool(
        description = "Set an importance weight (v0.8) for a file path, directory path, or \
                       classification category. weight=2.0 boosts; weight=0.1 suppresses. \
                       Applied multiplicatively to search RRF scores."
    )]
    async fn set_weight(
        &self,
        params: Parameters<SetWeightParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let SetWeightParams {
            target_kind,
            target,
            weight,
            reason,
        } = params.0;
        if weight < 0.0 {
            return Err(mcp_err("weight must be ≥ 0.0"));
        }
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        store
            .set_weight(&target_kind, &target, weight, "user", reason.as_deref())
            .map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Set {target_kind} weight for \"{target}\" = {weight:.2}"
        )))
    }

    /// Remove an importance weight.
    #[tool(
        description = "Remove an importance weight for a file, directory, or category, \
                       restoring it to neutral (1.0)."
    )]
    async fn delete_weight(
        &self,
        params: Parameters<DeleteWeightParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let DeleteWeightParams {
            target_kind,
            target,
        } = params.0;
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        store
            .delete_weight(&target_kind, &target)
            .map_err(mcp_err)?;
        Ok(ok_text(format!("Deleted weight for \"{target}\".")))
    }

    // ── Insights (v0.10) ───────────────────────────────────────────────────────

    /// Find duplicate or near-duplicate files in the index.
    #[tool(description = "Find duplicate files (v0.10 Insights). \
                       With exact=true, groups files with identical content hashes. \
                       With exact=false (default), groups files with similar summary embeddings \
                       above the similarity threshold.")]
    async fn insights_duplicates(
        &self,
        params: Parameters<InsightsDuplicatesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let InsightsDuplicatesParams { threshold, exact } = params.0;
        let exact = exact.unwrap_or(false);
        let threshold = threshold.unwrap_or(0.95).clamp(0.0, 1.0);
        let store = self.store()?;
        let clusters = if exact {
            store.find_exact_duplicates().map_err(mcp_err)?
        } else {
            store.find_near_duplicates(threshold).map_err(mcp_err)?
        };
        if clusters.is_empty() {
            return Ok(ok_text("No duplicates found."));
        }
        let lines: Vec<String> = clusters
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "Cluster {} ({} files, similarity {:.2}):\n{}",
                    i + 1,
                    c.paths.len(),
                    c.similarity,
                    c.paths
                        .iter()
                        .map(|p| format!("  {p}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
            .collect();
        Ok(ok_text(format!(
            "{} duplicate cluster(s):\n\n{}",
            clusters.len(),
            lines.join("\n\n")
        )))
    }

    /// Find stale directories (not modified for a long time).
    #[tool(
        description = "Find stale directories (v0.10 Insights) — not modified for more than \
                       `days` days (default 365). Helps identify inactive projects to archive."
    )]
    async fn insights_stale(
        &self,
        params: Parameters<InsightsDaysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let days = params.0.days.unwrap_or(365).max(1);
        let store = self.store()?;
        let stale = store.find_stale_entries(days).map_err(mcp_err)?;
        if stale.is_empty() {
            return Ok(ok_text(format!(
                "No stale directories found (threshold: {days} days)."
            )));
        }
        let lines: Vec<String> = stale
            .iter()
            .map(|s| format!("{} days ago: {}", s.days_since_modified, s.path))
            .collect();
        Ok(ok_text(format!(
            "{} stale director(ies) (>{days} days):\n\n{}",
            stale.len(),
            lines.join("\n")
        )))
    }

    /// Show what was added or modified in the index over the past N days.
    #[tool(
        description = "Show a diff of index changes (v0.10 Insights) — which files were newly \
                       discovered and which were modified on disk — over the past `days` days \
                       (default 7)."
    )]
    async fn insights_diff(
        &self,
        params: Parameters<InsightsDaysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let days = params.0.days.unwrap_or(7).max(1);
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - days * 86_400)
            .unwrap_or(0);
        let store = self.store()?;
        let diff = store.weekly_diff(since).map_err(mcp_err)?;
        let added_sample: Vec<&str> = diff.added.iter().take(20).map(|s| s.as_str()).collect();
        let modified_sample: Vec<&str> =
            diff.modified.iter().take(20).map(|s| s.as_str()).collect();
        Ok(ok_text(format!(
            "Index diff (last {days} day(s)):\n\nAdded ({}):\n{}\n\nModified ({}):\n{}",
            diff.added_count,
            if added_sample.is_empty() {
                "  none".to_owned()
            } else {
                added_sample
                    .iter()
                    .map(|p| format!("  + {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            diff.modified_count,
            if modified_sample.is_empty() {
                "  none".to_owned()
            } else {
                modified_sample
                    .iter()
                    .map(|p| format!("  ~ {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        )))
    }

    /// Index statistics (entry + chunk counts).
    #[tool(description = "Return index statistics: total indexed entries and embedded chunks.")]
    async fn get_stats(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let entries = store.entry_count().map_err(mcp_err)?;
        let chunks = store.chunk_count().map_err(mcp_err)?;
        Ok(ok_text(format!(
            "{entries} indexed entries, {chunks} chunks."
        )))
    }

    /// Shared raw-content reader used by `read_file` and `get_summary(tier=l2)`.
    ///
    /// Reads are **confined to files under an indexed root**. The tool contract is "an indexed
    /// file"; an MCP client must not be able to read arbitrary paths (`/etc/passwd`, `../../…`)
    /// through it. (Threat model is local stdio — the client already has the user's filesystem
    /// rights — so this is contract hygiene / defense-in-depth, not a privilege boundary.)
    fn read_file_inner(&self, path: &str) -> Result<CallToolResult, ErrorData> {
        let requested =
            std::fs::canonicalize(path).map_err(|e| mcp_err(format!("reading {path}: {e}")))?;
        let store = Store::open(&self.db_path).map_err(mcp_err)?;
        let roots: Vec<PathBuf> = store
            .root_paths()
            .map_err(mcp_err)?
            .iter()
            .filter_map(|r| std::fs::canonicalize(r).ok())
            .collect();
        if !path_within_roots(&requested, &roots) {
            return Err(mcp_err(format!(
                "path is not within an indexed root: {path}"
            )));
        }

        let bytes =
            std::fs::read(&requested).map_err(|e| mcp_err(format!("reading {path}: {e}")))?;
        let text = String::from_utf8_lossy(&bytes);
        let mut safe_end = text.len().min(READ_FILE_CAP);
        while safe_end > 0 && !text.is_char_boundary(safe_end) {
            safe_end -= 1;
        }
        let mut body = text[..safe_end].to_owned();
        if text.len() > safe_end {
            body.push_str("\n…[truncated]");
        }
        Ok(ok_text(body))
    }
}

#[tool_handler]
impl ServerHandler for IndexaMcp {
    fn get_info(&self) -> ServerInfo {
        // Identify as "indexa" (from_build_env() bakes in rmcp's own name/version).
        let mut server_info = Implementation::from_build_env();
        server_info.name = "indexa".to_owned();
        server_info.version = env!("CARGO_PKG_VERSION").to_owned();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(
            "Indexa is a local context engine: a hierarchically-summarized index of your files. \
             Navigate with `browse_tree` and `search`; call `get_summary` with tier=l0 (one-line \
             abstract) to scan cheaply, then drill to l1 (full summary) or l2 (raw content). \
             Use `read_file` for raw text; `ask` for grounded RAG answers (supports scope + mode). \
             Use `trigger_index` to index new or changed files. \
             Context Packs: `list_packs`/`get_pack`/`create_pack`/`add_pack_paths`/\
`remove_pack_paths`/`delete_pack`/`export_pack`/`search_pack` — \
             named, cross-directory bundles ready to paste into any AI tool. \
             Smart classification: `list_classifications`/`confirm_classification`/\
`ignore_classification`. \
             Code graph: `dependencies`/`who_imports`/`who_calls`/`blast_radius`."
                .to_owned(),
        )
    }
}

/// Run the Indexa MCP server over stdio until the client disconnects.
///
/// Logging must already be configured to stderr by the caller — stdout is the
/// JSON-RPC channel.
pub async fn serve_mcp(
    db_path: PathBuf,
    embedder: Arc<dyn Embedder + Send + Sync>,
    llm: Arc<dyn Generator + Send + Sync>,
    config: Config,
) -> Result<()> {
    let handler = IndexaMcp::new(db_path, embedder, llm, Arc::new(config));
    let service = handler.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Parse a user-supplied mode string into a `HybridMode`.
/// Accepts `"sparse"`, `"dense"`, `"rrf"` (default).
fn parse_hybrid_mode(s: Option<&str>) -> HybridMode {
    match s.unwrap_or("rrf").to_lowercase().as_str() {
        "sparse" => HybridMode::Sparse,
        "dense" => HybridMode::Dense,
        _ => HybridMode::Rrf,
    }
}

fn xml_escape_mcp(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// True if `requested` lies within any of the (canonicalized) indexed `roots`.
/// Uses component-wise [`Path::starts_with`], so the root `/home/u/proj` does NOT match
/// `/home/u/proj-evil` (a plain string-prefix check would wrongly accept it).
fn path_within_roots(requested: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| requested.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_within_roots_confines_to_index() {
        let roots = vec![PathBuf::from("/home/u/proj"), PathBuf::from("/data/notes")];
        // Inside a root → allowed.
        assert!(path_within_roots(
            Path::new("/home/u/proj/src/a.rs"),
            &roots
        ));
        assert!(path_within_roots(Path::new("/data/notes/x.md"), &roots));
        assert!(path_within_roots(Path::new("/home/u/proj"), &roots));
        // Outside every root → rejected.
        assert!(!path_within_roots(Path::new("/etc/passwd"), &roots));
        assert!(!path_within_roots(Path::new("/home/u/secret.txt"), &roots));
        // Sibling that merely shares a string prefix → rejected (component-wise match).
        assert!(!path_within_roots(Path::new("/home/u/proj-evil/x"), &roots));
        // No indexed roots → nothing is readable.
        assert!(!path_within_roots(Path::new("/home/u/proj/a"), &[]));
    }
}
