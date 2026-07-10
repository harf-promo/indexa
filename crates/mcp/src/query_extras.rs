//! Extra query tools: `project_overview`, `explain_retrieval`, and `inspect`.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_query::QaConfig;

use crate::{mcp_err, ok_text, parse_hybrid_mode, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectOverviewParams {
    /// Optional path prefix to scope the overview (omit for the whole index).
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExplainRetrievalParams {
    /// Natural-language question to trace through the retrieval pipeline.
    pub question: String,
    /// Optional path prefix to restrict retrieval.
    #[serde(default)]
    pub scope: Option<String>,
    /// Retrieval mode: `rrf` (default), `sparse`, or `dense`.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectPathParams {
    /// Absolute path to inspect (file or directory).
    pub path: String,
}

#[tool_router(router = router_query_extras, vis = "pub(crate)")]
impl IndexaMcp {
    /// Synthesize a plain-language overview of the indexed project (or a scoped subtree).
    #[tool(
        description = "Synthesize a plain-language overview of the whole indexed project (or the subtree at scope). Uses directory roll-up summaries to answer broad 'what is this project about?' questions. Much faster than ask for project-level context.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn project_overview(
        &self,
        params: Parameters<ProjectOverviewParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let scope = params.0.scope.filter(|s| !s.is_empty());
        let store = self.store()?;
        // When no scope is given, use the first indexed root so the function can
        // find the root directory summary without any search hits.
        let roots = store.root_paths().map_err(mcp_err)?;
        let resolved_scope: Option<String> = scope.or_else(|| roots.into_iter().next());
        let overview =
            indexa_query::build_project_overview(&store, &[], resolved_scope.as_deref(), 2000);
        if overview.is_empty() {
            Ok(ok_text(
                "No project overview available — run `indexa summarize` first to build directory roll-ups.",
            ))
        } else {
            Ok(ok_text(overview))
        }
    }

    /// Return the full retrieval trace for a question (sparse / dense / fused stages with scores).
    #[tool(
        description = "Return the full retrieval trace for a question: which sparse/dense/fused stages ran, top-k scores per stage, and why the top sources were selected. Use to understand or debug why ask returned specific sources.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn explain_retrieval(
        &self,
        params: Parameters<ExplainRetrievalParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let ExplainRetrievalParams {
            question,
            scope,
            mode,
        } = params.0;
        // Same config-derived defaults as MCP `ask` (via the shared constructor), so
        // `explain_retrieval` can't drift from what `ask` actually runs.
        let mut cfg = QaConfig::from_retrieval(&self.config.retrieval);
        if let Some(m) = mode.as_deref() {
            cfg.mode = parse_hybrid_mode(Some(m));
        }
        cfg.scope = scope.filter(|s| !s.is_empty());

        // Trace what `ask` actually runs, including the ANN dense arm when it's active.
        let ann = self.ensure_ann().await;
        let trace = indexa_query::explain_retrieval(
            &self.db_path,
            self.embedder.as_ref(),
            self.llm.as_ref(),
            &question,
            &cfg,
            ann.as_deref(),
        )
        .await
        .map_err(mcp_err)?;

        // Format the trace as human-readable text (RetrievalTrace doesn't derive Serialize).
        let mut out = format!(
            "Retrieval trace for: {}\nmode={} top_k={} rrf_k={:.0} rerank={} use_weights={}",
            trace.question, trace.mode, trace.top_k, trace.rrf_k, trace.rerank, trace.use_weights,
        );
        if let Some(s) = &trace.scope {
            out.push_str(&format!(" scope={s}"));
        }
        out.push('\n');
        for stage in &trace.stages {
            out.push_str(&format!(
                "\n## {} ({} hits)\n",
                stage.label,
                stage.hits.len()
            ));
            for (i, h) in stage.hits.iter().enumerate().take(10) {
                let heading = if h.heading.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", h.heading)
                };
                out.push_str(&format!(
                    "  {}. {}{} score={:.4}\n",
                    i + 1,
                    h.entry_path,
                    heading,
                    h.rrf_score,
                ));
            }
            if stage.hits.len() > 10 {
                out.push_str(&format!("  … {} more\n", stage.hits.len() - 10));
            }
        }

        Ok(ok_text(out))
    }

    /// Return indexed facts about a path: kind, size, chunk count, summary, category, weight, edges.
    #[tool(
        description = "Return indexed facts about a path: kind, size, modification time, chunk count, language, summary, model, classification category, importance weight, and outgoing code-graph edges. The same data the 'Indexed facts' panel shows in the web UI.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn inspect(
        &self,
        params: Parameters<InspectPathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = params.0.path;
        let store = self.store()?;

        let entry = store.entry_by_path(&path).ok().flatten();
        let summary = store.summary_by_path(&path).ok().flatten();
        let chunks = store.chunks_for_path(&path, 0).unwrap_or_default();

        if entry.is_none() && summary.is_none() && chunks.is_empty() {
            return Ok(ok_text(format!(
                "Nothing indexed at {path}. Run `indexa index <path>` to index it."
            )));
        }

        let classification = store.classification_for(&path).ok().flatten();
        let weight = store.weight_for(&path).unwrap_or(1.0);
        let edges = store.edges_from(&path).unwrap_or_default();

        let count_kind = |k: &str| edges.iter().filter(|e| e.kind == k).count();
        let language = chunks.iter().find_map(|c| c.language.clone());

        let mut out = format!("Indexed facts for {path}\n");

        // Entry facts
        if let Some(ref e) = entry {
            out.push_str(&format!("  kind:     {}\n", e.kind));
            out.push_str(&format!("  size:     {} bytes\n", e.size));
            if let Some(ms) = e.modified_s {
                out.push_str(&format!("  modified: {ms}\n"));
            }
        }

        // Chunk facts
        out.push_str(&format!("  chunks:   {}\n", chunks.len()));
        if let Some(ref lang) = language {
            out.push_str(&format!("  language: {lang}\n"));
        }

        // Summary facts
        if let Some(ref s) = summary {
            out.push_str(&format!("  summary model: {}\n", s.model));
            if let Some(ref l0) = s.summary_l0 {
                out.push_str(&format!("  abstract: {l0}\n"));
            }
        } else {
            out.push_str("  summary:  (none — run `indexa summarize`)\n");
        }

        // Classification
        if let Some(ref c) = classification {
            out.push_str(&format!(
                "  category: {} (confidence {:.2}, source: {})\n",
                c.category, c.confidence, c.source
            ));
        }

        // Detected application/structure (v0.66): what kind of thing this directory is.
        let apps = store.apps_for_dir(&path).unwrap_or_default();
        if let Some(primary) = apps.iter().find(|a| a.is_primary).or_else(|| apps.first()) {
            let others: Vec<&str> = apps
                .iter()
                .filter(|a| a.app_kind != primary.app_kind)
                .map(|a| a.app_name.as_str())
                .collect();
            let also = if others.is_empty() {
                String::new()
            } else {
                format!(" (also: {})", others.join(", "))
            };
            out.push_str(&format!(
                "  app:      {} [{}]{also}\n",
                primary.app_name, primary.family
            ));
        }

        // Importance weight
        out.push_str(&format!("  weight:   {weight:.2}\n"));

        // Code-graph edges
        let imports = count_kind("imports");
        let defines = count_kind("defines");
        let calls = count_kind("calls");
        if imports + defines + calls > 0 {
            out.push_str(&format!(
                "  edges:    imports={imports} defines={defines} calls={calls}\n"
            ));
        }

        Ok(ok_text(out))
    }
}
