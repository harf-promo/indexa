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
