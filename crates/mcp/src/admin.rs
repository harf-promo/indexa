//! Admin tools: `get_stats`, `prune`, and `trigger_index`.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use crate::{mcp_err, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TriggerIndexParams {
    /// Absolute path to scan, deep-index, and summarize.
    pub path: String,
}

#[tool_router(router = router_admin, vis = "pub(crate)")]
impl IndexaMcp {
    /// Index statistics (entry + chunk counts).
    #[tool(description = "Return index statistics: total indexed entries and embedded chunks.")]
    pub(crate) async fn get_stats(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let entries = store.entry_count().map_err(mcp_err)?;
        let chunks = store.chunk_count().map_err(mcp_err)?;
        let mut out = format!("{entries} indexed entries, {chunks} chunks.");
        // Measured token savings (approximate by definition — see store::usage);
        // best-effort, so a telemetry read failure can't fail the stats call.
        if let Some(line) = store
            .usage_summary(indexa_core::store::USAGE_WEEK_SECS)
            .ok()
            .and_then(|u| u.savings_line())
        {
            out.push('\n');
            out.push_str(&line);
        }
        Ok(ok_text(out))
    }

    /// Report the effective Indexa configuration (models, retrieval, scan) — no secrets.
    #[tool(
        description = "Return Indexa's effective configuration: embedding + describer models, \
                       retrieval defaults (mode, top_k, agentic), chunking, scan ignore rules, \
                       and parser caps. API keys are NEVER included. Read-only — use it to \
                       understand how retrieval is tuned before asking or searching."
    )]
    pub(crate) async fn query_config(&self) -> Result<CallToolResult, ErrorData> {
        let c = &self.config;
        // Deliberately excludes `api_keys` — secrets are never returned over a tool.
        let mode = format!("{:?}", c.retrieval.hybrid).to_lowercase();
        let out = format!(
            "Embedding: {} / {} (dim {})\n\
             Describer: {} / {} (file: {}, dir: {}; passes first/refresh: {}/{})\n\
             Retrieval: mode={mode}, top_k={}, rerank={}, agentic={} (max {} steps), \
             use_weights={}, context_budget={} bytes\n\
             Chunking:  {:?}, size {}, overlap {}\n\
             Scan:      respect_gitignore={}, auto_reindex={}, ignore=[{}]\n\
             Parsers:   max_file_mb={}, pdf_backend={}, image_caption={}, \
             audio_transcribe={}, video_caption={}",
            c.embedding.provider,
            c.embedding.model,
            c.embedding.dim,
            c.describer.provider,
            c.describer.model,
            c.describer.file_model,
            c.describer.dir_model,
            c.describer.passes_first,
            c.describer.passes_refresh,
            c.retrieval.top_k,
            c.retrieval.rerank,
            c.retrieval.agentic,
            c.retrieval.agentic_max_steps,
            c.retrieval.use_weights,
            c.retrieval.context_budget,
            c.chunking.strategy,
            c.chunking.size,
            c.chunking.overlap,
            c.scan.respect_gitignore,
            c.scan.auto_reindex,
            c.scan.ignore.join(", "),
            c.parsers.max_file_mb,
            c.parsers.pdf.backend,
            c.parsers.image.caption,
            c.parsers.audio.transcribe,
            c.parsers.video.caption,
        );
        Ok(ok_text(out))
    }

    /// Garbage-collect orphaned rows (chunks/summaries left behind after a root was removed).
    #[tool(
        description = "Garbage-collect orphaned index rows — chunks and summaries left behind after their files/roots were removed. Returns how many rows were pruned. Safe: only removes rows with no matching entry."
    )]
    pub(crate) async fn prune(&self) -> Result<CallToolResult, ErrorData> {
        let mut store = self.store()?;
        let counts = store.prune_orphans().map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Pruned {} orphaned chunk(s) and {} orphaned summary row(s).",
            counts.chunks, counts.summaries
        )))
    }

    /// Trigger a full scan → deep-index → summarize pipeline on a path.
    #[tool(
        description = "Start an `indexa index <path>` run: scan files, compute embeddings, \
                       and generate summaries. Runs as a background subprocess and returns \
                       when indexing is complete. Use before asking questions about new or \
                       changed files."
    )]
    pub(crate) async fn trigger_index(
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
}
