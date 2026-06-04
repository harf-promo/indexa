//! MCP (Model Context Protocol) server exposing the Indexa index to AI agents.
//!
//! Started via `indexa mcp`, it speaks JSON-RPC over **stdio** so clients like
//! Claude Desktop and Cursor can browse the local index live as tool calls. It
//! reuses the existing `Store` and `query` functions directly — no HTTP layer.
//!
//! **stdout is the protocol channel** — all logging must go to stderr.
//!
//! Tools (13): `search`, `browse_tree`, `get_summary` (tier l0/l1/l2 — progressive
//! disclosure), `read_file`, `ask`, `dependencies` (a file's imports, defined symbols,
//! and calls), `who_imports` (reverse code-graph lookup), `who_calls` (D2 — reverse
//! call lookup), `blast_radius` (D2 — 1-hop call blast radius), `get_stats`,
//! `list_packs`, `get_pack`, `export_pack` (Context Packs).

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
        } = params.0;
        let limit = limit.unwrap_or(20).min(100);
        let scope = scope.as_deref().filter(|s| !s.is_empty());

        // Try to embed the query for the dense arm; fall back to sparse if the embedder is
        // unavailable or the index has no embeddings.
        let embedding = self.embedder.embed(&query).await.ok();

        let store = self.store()?;
        let hits = store
            .hybrid_search(
                &query,
                embedding.as_deref(),
                &HybridMode::Rrf,
                scope,
                limit,
                60.0,
            )
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
        let question = params.0.question;
        let cfg = QaConfig {
            top_k: self.config.retrieval.top_k,
            mode: self.config.retrieval.hybrid.clone(),
            context_budget: self.config.retrieval.context_budget,
            rrf_k: self.config.retrieval.rrf_k as f32,
            summary_weight: self.config.retrieval.summary_weight,
            summary_depth_alpha: self.config.retrieval.summary_depth_alpha,
            rerank: self.config.retrieval.rerank,
            ..QaConfig::default()
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
             abstract) to scan cheaply, then drill to l1 (full summary) or l2 (raw content). Use \
             `read_file` for raw text and `ask` for grounded question-answering over the index. \
             Use `list_packs` / `get_pack` / `export_pack` to work with Context Packs — named, \
             cross-directory context bundles ready to paste into any AI tool."
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
