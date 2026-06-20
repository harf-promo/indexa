//! Retrieval tools: `search`, `browse_tree`, `get_summary` (l0/l1/l2),
//! `read_file`, and `ask`.

use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::{config::HybridMode, store::Store};
use indexa_query::{PriorTurn, QaConfig};

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
    /// byte offset to start reading from (for paging past the 40 KB cap); default 0
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetChunkContextParams {
    /// Absolute path of an indexed file.
    pub path: String,
    /// Center chunk sequence number (e.g. a `search` hit's position). Omit to
    /// return the file's first chunks.
    #[serde(default)]
    pub seq: Option<usize>,
    /// Neighbor chunks to include on each side of `seq` (default 1). With `seq`
    /// omitted, the first `2*radius+1` chunks are returned instead.
    #[serde(default)]
    pub radius: Option<usize>,
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
    /// Enable cross-encoder reranking after retrieval (adds latency; improves ranking quality).
    /// Defaults to the server's `[retrieval] rerank`. Use with `rerank_backend` to choose
    /// between `"llm"` (listwise, default) or `"cross-encoder"` (candle DeBERTa-v2).
    #[serde(default)]
    pub rerank: Option<bool>,
    /// Reranker backend when `rerank` is true: `"llm"` (default) or `"cross-encoder"`.
    #[serde(default)]
    pub rerank_backend: Option<String>,
    /// Conversational Ask: an opaque conversation id. Pass the SAME id across calls to make
    /// a multi-turn conversation — the server folds the session's recent turns into the
    /// prompt, rewrites the follow-up into a standalone query, and records this turn. Omit
    /// for a stateless single-shot answer (the default). Generate any stable string (e.g. a
    /// UUID) per conversation.
    #[serde(default)]
    pub session_id: Option<String>,
    /// retrieval breadth — chunks fetched before synthesis; default from server config
    #[serde(default)]
    pub top_k: Option<usize>,
}

#[tool_router(router = router_retrieval, vis = "pub(crate)")]
impl IndexaMcp {
    /// Hybrid keyword + semantic search over indexed **chunk content** (BM25 + vector RRF).
    /// Returns matching chunks with their file path, heading, and a text snippet.
    /// Use `scope` to restrict to a subtree. For path-name browsing, prefer `browse_tree`.
    #[tool(
        description = "Search indexed chunk content by keyword (BM25 + vector hybrid). Returns matching chunks with path, heading, and snippet — richer than path-name search. Each hit shows `#N`, the chunk seq — pass it to `get_chunk_context` to expand that chunk with its neighbors. Optionally scope to a path prefix."
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
                format!("{}{} #{}\n  {}", h.entry_path, heading, h.seq, snippet)
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
            return self.read_file_inner(&path, 0, "get_summary");
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
    #[tool(
        description = "Read the raw text content of an indexed file (truncated to ~40 KB). Pass `offset` (a byte offset) to page past the cap and read a later window of a large file."
    )]
    pub(crate) async fn read_file(
        &self,
        params: Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.read_file_inner(&params.0.path, params.0.offset.unwrap_or(0), "read_file")
    }

    /// Return the indexed chunks of a file, optionally a window around one `seq`.
    #[tool(
        description = "Return a file's indexed chunks (the exact text Indexa retrieves over), \
                       with seq number and heading. Pass `seq` (a search hit's position) to get \
                       that chunk plus `radius` neighbors on each side — the surrounding context \
                       a snippet alone omits. Omit `seq` for the file's opening chunks."
    )]
    pub(crate) async fn get_chunk_context(
        &self,
        params: Parameters<GetChunkContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let GetChunkContextParams { path, seq, radius } = params.0;
        let radius = radius.unwrap_or(1);
        let mut store = self.store()?;
        let chunks = store.chunks_for_path(&path, 0).map_err(mcp_err)?;
        if chunks.is_empty() {
            return Ok(ok_text(format!(
                "No indexed chunks for {path}. Run `indexa deep <path>` to make it searchable."
            )));
        }
        let window: Vec<_> = match seq {
            Some(center) => {
                let lo = center.saturating_sub(radius);
                let hi = center.saturating_add(radius);
                chunks
                    .iter()
                    .filter(|c| c.seq >= lo && c.seq <= hi)
                    .collect()
            }
            None => chunks.iter().take(2 * radius + 1).collect(),
        };
        if window.is_empty() {
            return Ok(ok_text(format!(
                "No chunk near seq {} in {path} (the file has {} chunk(s), seq 0..{}).",
                seq.unwrap_or(0),
                chunks.len(),
                chunks.len().saturating_sub(1)
            )));
        }
        let body = window
            .iter()
            .map(|c| {
                let heading = if c.heading.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", c.heading)
                };
                format!("#{}{}\n{}", c.seq, heading, c.text)
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        let out = format!("{} chunk(s) from {path}:\n\n{body}", window.len());

        // Served = bytes returned; counterfactual = the file's full on-disk size
        // (same basis as read_file — see store::usage for the honest definition).
        let counterfactual = store.counterfactual_bytes_for_paths(&[&path]).unwrap_or(0);
        record_usage(&mut store, "get_chunk_context", out.len(), counterfactual);

        Ok(ok_text(out))
    }

    /// Answer a natural-language question against the index (grounded RAG).
    #[tool(
        description = "Answer a natural-language question using the indexed context (hybrid retrieval + local LLM synthesis). Returns an answer with source paths. Set `agentic: true` for compositional questions — the model plans and refines the search across several hops before answering (a few extra model calls). Optional `top_k` widens/narrows retrieval breadth."
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
            rerank,
            rerank_backend,
            session_id,
            top_k,
        } = params.0;
        let agentic = agentic.unwrap_or(self.config.retrieval.agentic);
        let cfg = QaConfig {
            top_k: top_k
                .map(|k| k.min(100))
                .unwrap_or(self.config.retrieval.top_k),
            mode: mode
                .as_deref()
                .map(|m| parse_hybrid_mode(Some(m)))
                .unwrap_or_else(|| self.config.retrieval.hybrid.clone()),
            scope: scope.filter(|s| !s.is_empty()),
            context_budget: self.config.retrieval.context_budget,
            rrf_k: self.config.retrieval.rrf_k as f32,
            summary_weight: self.config.retrieval.summary_weight,
            summary_depth_alpha: self.config.retrieval.summary_depth_alpha,
            rerank: rerank.unwrap_or(self.config.retrieval.rerank),
            rerank_backend: rerank_backend
                .unwrap_or_else(|| self.config.retrieval.rerank_backend.clone()),
            use_weights: self.config.retrieval.use_weights,
            use_recency_weight: self.config.retrieval.recency_boost,
            recency_days: self.config.retrieval.recency_days,
            max_steps: self.config.retrieval.agentic_max_steps,
            mmr_lambda: self.config.retrieval.mmr_lambda,
            archive_segments: self.config.retrieval.archive_segments.clone(),
            archive_penalty: self.config.retrieval.archive_penalty,
        };

        // Conversational Ask: when a session id is given, load its recent turns (fail-open;
        // empty for a stateless ask) so the pipeline can rewrite the follow-up + fold context.
        let history =
            load_session_history(&self.db_path, session_id.as_deref(), cfg.scope.as_deref());

        // Single shared, Send-safe pipeline (embed → scoped retrieve → optional
        // rerank → synthesize). The `_history` entry points open their own short-lived read
        // connection from `db_path`; the empty-hit short-circuit lives inside. Agentic mode
        // adds a bounded plan→search→refine loop before synthesis and records the queries it
        // ran so the agent can see how the answer was gathered.
        let mut steps: Vec<String> = Vec::new();
        let answer = if agentic {
            indexa_query::answer_agentic_history(
                &self.db_path,
                self.embedder.as_ref(),
                self.llm.as_ref(),
                &question,
                &cfg,
                &history,
                &mut |_step, query| steps.push(query.to_owned()),
            )
            .await
            .map_err(mcp_err)?
        } else {
            indexa_query::answer_with_ann_history(
                &self.db_path,
                self.embedder.as_ref(),
                self.llm.as_ref(),
                &question,
                &cfg,
                None,
                &history,
            )
            .await
            .map_err(mcp_err)?
        };

        // Clone for the decorated tool output; the original `answer` is kept whole so the
        // persisted turn stores the clean answer text (without the Sources/Confidence footer).
        let mut out = answer.answer.clone();
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
            out.push_str(&format!(
                "\nRetrieval coverage: {} ({})\n",
                c.level, c.basis
            ));
            // Heuristic coverage gap: question terms absent from every cited source.
            if let Some(gaps) = c.uncovered.as_ref().filter(|g| !g.is_empty()) {
                out.push_str(&format!("Possibly uncovered: {}\n", gaps.join(", ")));
            }
        }
        // Conversational Ask: tell the agent which id to reuse to continue the conversation.
        if let Some(id) = &session_id {
            out.push_str(&format!(
                "\nConversation: {id} (pass the same session_id to follow up)\n"
            ));
        }

        if let Ok(mut store) = Store::open(&self.db_path) {
            let paths: Vec<&str> = answer.sources.iter().map(|s| s.path.as_str()).collect();
            let counterfactual = store.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
            // Show the agent the same "retrieve the slice" win the CLI and web surfaces print —
            // only when meaningful (cited files existed AND serving was smaller), never a
            // misleading "0% saved". Appended before record_usage so the readout is counted too.
            let imp = indexa_query::AnswerImpact::new(out.len() as u64, counterfactual);
            if imp.is_meaningful() {
                out.push_str(&format!("\nImpact: {}\n", imp.human()));
            }
            record_usage(&mut store, "ask", out.len(), counterfactual);
            // Persist the turn (best-effort; never fails the answer).
            if let Some(id) = &session_id {
                append_session_turn(&mut store, id, &question, &answer);
            }
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
        offset: usize,
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
        // Page within the file: snap the requested offset DOWN to a char boundary, then serve a
        // READ_FILE_CAP-wide window. `offset > 0` pages past the cap into a later slice.
        let start = indexa_core::text::floor_char_boundary(&text, offset);
        let end =
            indexa_core::text::floor_char_boundary(&text, start.saturating_add(READ_FILE_CAP));
        let mut body = String::new();
        if start > 0 {
            body.push_str(&format!("…[{start} bytes before]\n"));
        }
        body.push_str(&text[start..end]);
        if end < text.len() {
            body.push_str("\n…[truncated]");
        }

        // Counterfactual = the file's full on-disk size (vs. the served window).
        record_usage(&mut store, tool, body.len(), bytes.len() as u64);

        Ok(ok_text(body))
    }
}

/// How many recent turns of a conversation to fold into an MCP `ask` (matches the web surface).
const ASK_HISTORY_TURNS: usize = 6;

/// Load a conversation's recent turns for the qa pipeline (fail-open: any error ⇒ no history,
/// i.e. a stateless answer). `None` session_id ⇒ empty.
fn load_session_history(
    db_path: &std::path::Path,
    session_id: Option<&str>,
    scope: Option<&str>,
) -> Vec<PriorTurn> {
    let Some(id) = session_id else {
        return Vec::new();
    };
    let Ok(mut store) = Store::open(db_path) else {
        return Vec::new();
    };
    if store.ensure_session(id, scope).is_err() {
        return Vec::new();
    }
    store
        .recent_turns(id, ASK_HISTORY_TURNS)
        .map(|turns| {
            turns
                .into_iter()
                .map(|t| PriorTurn {
                    question: t.question,
                    answer: t.answer,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Persist a completed turn, best-effort (serializes citations to the opaque `sources_json`).
fn append_session_turn(
    store: &mut Store,
    session_id: &str,
    question: &str,
    answer: &indexa_query::Answer,
) {
    let sources_json = serde_json::to_string(
        &answer
            .sources
            .iter()
            .map(|s| {
                serde_json::json!({ "path": s.path, "heading": s.heading, "snippet": s.snippet })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned());
    let _ = store.append_turn(session_id, question, &answer.answer, &sources_json);
}
