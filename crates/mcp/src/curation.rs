//! Curation tools: Smart classification, importance weights, and saved
//! searches.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::store::Store;

use crate::{mcp_err, ok_text, IndexaMcp};

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

#[tool_router(router = router_curation, vis = "pub(crate)")]
impl IndexaMcp {
    // ── Smart classification ───────────────────────────────────────────────────

    /// List auto-suggested or user-confirmed classifications.
    #[tool(
        description = "List Smart classification records — auto-detected or user-confirmed \
                       category labels for files and directories. Filter by source: `auto`, \
                       `user`, or `ignored`."
    )]
    pub(crate) async fn list_classifications(
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
    pub(crate) async fn confirm_classification(
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
    pub(crate) async fn ignore_classification(
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
    pub(crate) async fn list_weights(&self) -> Result<CallToolResult, ErrorData> {
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

    /// List the user's saved searches.
    #[tool(
        description = "List the user's saved searches: named, reusable ask queries (question + \
                       retrieval mode + optional path scope), managed via `indexa saved` or the \
                       web Ask bar. Re-run one by passing its question (and scope) to the `ask` \
                       tool — use mode 'agentic' as agentic: true."
    )]
    pub(crate) async fn list_saved_queries(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let queries = store.list_saved_queries().map_err(mcp_err)?;
        if queries.is_empty() {
            return Ok(ok_text(
                "No saved searches. Create one with `indexa saved add` or the web Ask bar's ☆.",
            ));
        }
        let lines: Vec<String> = queries
            .iter()
            .map(|q| {
                let scope = q
                    .scope
                    .as_deref()
                    .map(|s| format!(", scope: {s}"))
                    .unwrap_or_default();
                format!(
                    "• {} — \"{}\" (mode: {}{scope})",
                    q.name, q.question, q.mode
                )
            })
            .collect();
        Ok(ok_text(format!(
            "{} saved search(es):\n\n{}",
            queries.len(),
            lines.join("\n")
        )))
    }

    /// Set an importance weight for a file, directory, or category.
    #[tool(
        description = "Set an importance weight (v0.8) for a file path, directory path, or \
                       classification category. weight=2.0 boosts; weight=0.1 suppresses. \
                       Applied multiplicatively to search RRF scores."
    )]
    pub(crate) async fn set_weight(
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
    pub(crate) async fn delete_weight(
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
}
