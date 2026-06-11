//! Retrieval tools: `search`, `browse_tree`, `get_summary` (l0/l1/l2),
//! `read_file`, and `ask`.

use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::{config::HybridMode, store::Store};
use indexa_query::QaConfig;

use crate::{
    mcp_err, ok_text, parse_hybrid_mode, path_within_roots, record_usage, IndexaMcp, READ_FILE_CAP,
};

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
    /// Agentic multi-hop retrieval: the model plans and refines the search across
    /// several hops before answering. Better for compositional questions; costs a
    /// few extra model calls. Defaults to the server's `[retrieval] agentic`.
    #[serde(default)]
    pub agentic: Option<bool>,
}

#[tool_router(router = router_retrieval, vis = "pub(crate)")]
impl IndexaMcp {
    /// Hybrid keyword + semantic search over indexed **chunk content** (BM25 + vector RRF).
    /// Returns matching chunks with their file path, heading, and a text snippet.
    /// Use `scope` to restrict to a subtree. For path-name browsing, prefer `browse_tree`.
    #[tool(
        description = "Search indexed chunk content by keyword (BM25 + vector hybrid). Returns matching chunks with path, heading, and snippet — richer than path-name search. Optionally scope to a path prefix."
    )]
    pub(crate) async fn search(
        &self,
        params: Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
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

        let mut store = self.store()?;
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
        let out = format!("{} result(s):\n\n{body}", hits.len());

        let paths: Vec<&str> = hits.iter().map(|h| h.entry_path.as_str()).collect();
        let counterfactual = store.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
        record_usage(&mut store, "search", out.len(), counterfactual);

        Ok(ok_text(out))
    }

    /// List the direct children (with summary state) of a directory.
    #[tool(
        description = "List the direct children of a directory in the index, with each child's kind and file/chunk counts. Empty path lists indexed roots."
    )]
    pub(crate) async fn browse_tree(
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
    pub(crate) async fn get_summary(
        &self,
        params: Parameters<GetSummaryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let GetSummaryParams { path, tier } = params.0;
        let tier = tier.as_deref().unwrap_or("l1").to_lowercase();

        if tier == "l2" {
            return self.read_file_inner(&path, "get_summary");
        }

        let mut store = self.store()?;
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

        let counterfactual = store.counterfactual_bytes_for_paths(&[&path]).unwrap_or(0);
        record_usage(&mut store, "get_summary", out.len(), counterfactual);

        Ok(ok_text(out))
    }

    /// Read raw file content (L2).
    #[tool(description = "Read the raw text content of an indexed file (truncated to ~40 KB).")]
    pub(crate) async fn read_file(
        &self,
        params: Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.read_file_inner(&params.0.path, "read_file")
    }

    /// Answer a natural-language question against the index (grounded RAG).
    #[tool(
        description = "Answer a natural-language question using the indexed context (hybrid retrieval + local LLM synthesis). Returns an answer with source paths. Set `agentic: true` for compositional questions — the model plans and refines the search across several hops before answering (a few extra model calls)."
    )]
    pub(crate) async fn ask(
        &self,
        params: Parameters<AskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let AskParams {
            question,
            scope,
            mode,
            agentic,
        } = params.0;
        let agentic = agentic.unwrap_or(self.config.retrieval.agentic);
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
            max_steps: self.config.retrieval.agentic_max_steps,
        };

        // Single shared, Send-safe pipeline (embed → scoped retrieve → optional
        // rerank → synthesize). `answer` opens its own short-lived read connection
        // from `db_path`; the empty-hit short-circuit lives inside it. Agentic mode
        // adds a bounded plan→search→refine loop before synthesis and records the
        // queries it ran so the agent can see how the answer was gathered.
        let mut steps: Vec<String> = Vec::new();
        let answer = if agentic {
            indexa_query::answer_agentic(
                &self.db_path,
                self.embedder.as_ref(),
                self.llm.as_ref(),
                &question,
                &cfg,
                &mut |_step, query| steps.push(query.to_owned()),
            )
            .await
            .map_err(mcp_err)?
        } else {
            indexa_query::answer(
                &self.db_path,
                self.embedder.as_ref(),
                self.llm.as_ref(),
                &question,
                &cfg,
            )
            .await
            .map_err(mcp_err)?
        };

        let mut out = answer.answer;
        if !answer.sources.is_empty() {
            out.push_str("\n\nSources:\n");
            for s in &answer.sources {
                out.push_str(&format!("- {}\n", s.path));
            }
        }
        if agentic && steps.len() > 1 {
            out.push_str("\nRetrieval steps:\n");
            for (i, q) in steps.iter().enumerate() {
                out.push_str(&format!("- {}. {}\n", i + 1, q));
            }
        }
        // Retrieval-shape confidence (heuristic, from the hit pool — not calibrated).
        // Absent for the no-match short-circuit, whose message stands on its own.
        if let Some(c) = &answer.confidence {
            out.push_str(&format!("\nConfidence: {} ({})\n", c.level, c.basis));
        }

        if let Ok(mut store) = Store::open(&self.db_path) {
            let paths: Vec<&str> = answer.sources.iter().map(|s| s.path.as_str()).collect();
            let counterfactual = store.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
            record_usage(&mut store, "ask", out.len(), counterfactual);
        }

        Ok(ok_text(out))
    }

    /// Shared raw-content reader used by `read_file` and `get_summary(tier=l2)`.
    ///
    /// Reads are **confined to files under an indexed root**. The tool contract is "an indexed
    /// file"; an MCP client must not be able to read arbitrary paths (`/etc/passwd`, `../../…`)
    /// through it. (Threat model is local stdio — the client already has the user's filesystem
    /// rights — so this is contract hygiene / defense-in-depth, not a privilege boundary.)
    pub(crate) fn read_file_inner(
        &self,
        path: &str,
        tool: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let requested =
            std::fs::canonicalize(path).map_err(|e| mcp_err(format!("reading {path}: {e}")))?;
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
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

        // Counterfactual = the file's full on-disk size (vs. the served cap).
        record_usage(&mut store, tool, body.len(), bytes.len() as u64);

        Ok(ok_text(body))
    }
}
