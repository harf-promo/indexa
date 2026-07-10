//! Insights tools: duplicates, stale directories, index diff, largest files,
//! and language breakdown.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use crate::{mcp_err, ok_text, IndexaMcp};

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InsightsLargestParams {
    /// How many of the largest files to return (default 20, max 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[tool_router(router = router_insights, vis = "pub(crate)")]
impl IndexaMcp {
    // ── Insights (v0.10) ───────────────────────────────────────────────────────

    /// Find duplicate or near-duplicate files in the index.
    #[tool(description = "Find duplicate files (v0.10 Insights). \
                       With exact=true, groups files with identical content hashes. \
                       With exact=false (default), groups files with similar summary embeddings \
                       (approximate above ~2,000 files via LSH — borderline pairs may be \
                       missed; exact-duplicate grouping is exhaustive) \
                       above the similarity threshold.")]
    pub(crate) async fn insights_duplicates(
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
        // Cap the clusters listed so a heavily-duplicated tree can't flood the client's context;
        // report the true total and how many are shown.
        const MAX_CLUSTERS: usize = 50;
        let total = clusters.len();
        let lines: Vec<String> = clusters
            .iter()
            .take(MAX_CLUSTERS)
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
        let header = if total > MAX_CLUSTERS {
            format!("{total} duplicate cluster(s) (showing first {MAX_CLUSTERS}):")
        } else {
            format!("{total} duplicate cluster(s):")
        };
        Ok(ok_text(format!("{header}\n\n{}", lines.join("\n\n"))))
    }

    /// Find stale directories (not modified for a long time).
    #[tool(
        description = "Find stale directories (v0.10 Insights) — not modified for more than \
                       `days` days (default 365). Helps identify inactive projects to archive."
    )]
    pub(crate) async fn insights_stale(
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
    pub(crate) async fn insights_diff(
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

    /// Largest indexed files (bloat detection).
    #[tool(
        description = "List the largest indexed files by on-disk size (bloat detection). `limit` defaults to 20."
    )]
    pub(crate) async fn insights_largest(
        &self,
        params: Parameters<InsightsLargestParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = params.0.limit.unwrap_or(20).clamp(1, 500);
        let store = self.store()?;
        let rows = store.find_largest(limit).map_err(mcp_err)?;
        if rows.is_empty() {
            return Ok(ok_text("No indexed files found.".to_owned()));
        }
        let body = rows
            .iter()
            .map(|e| format!("{} bytes  {}", e.size, e.path))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(format!("Largest {} file(s):\n{body}", rows.len())))
    }

    /// Language breakdown of indexed content.
    #[tool(
        description = "Show the language breakdown of indexed content (chunk count per language). Only code chunks carry a language tag."
    )]
    pub(crate) async fn insights_languages(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let rows = store.language_breakdown().map_err(mcp_err)?;
        if rows.is_empty() {
            return Ok(ok_text(
                "No language-tagged chunks yet. Run `indexa deep` on source files first."
                    .to_owned(),
            ));
        }
        let total: u64 = rows.iter().map(|l| l.chunks).sum();
        let body = rows
            .iter()
            .map(|l| {
                let pct = if total > 0 {
                    l.chunks as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                format!("{:>6.1}%  {} ({} chunks)", pct, l.language, l.chunks)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(format!(
            "Language breakdown ({total} chunks):\n{body}"
        )))
    }
}
