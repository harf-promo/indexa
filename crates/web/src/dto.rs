//! Request/response DTOs, small helpers, and conversions shared by the handlers.

use crate::jobs::JobStatus;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── API types ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct StatsResponse {
    pub(crate) entries: u64,
    pub(crate) chunks: u64,
}

#[derive(Serialize)]
pub(crate) struct MapRow {
    pub(crate) category: String,
    pub(crate) entry_count: u64,
    pub(crate) total_size: u64,
}

#[derive(Serialize)]
pub(crate) struct TreeNodeResponse {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) child_count: i64,
    pub(crate) byte_size: i64,
    pub(crate) summary_state: Option<String>,
    pub(crate) file_count: i64,
    pub(crate) chunk_count: i64,
}

#[derive(Serialize)]
pub(crate) struct SummaryChildResponse {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    #[serde(rename = "abstract")]
    pub(crate) abstract_: String,
    pub(crate) summary: String,
    pub(crate) summary_state: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct BreadcrumbResponse {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) summary: String,
}

#[derive(Serialize)]
pub(crate) struct SummaryResponse {
    pub(crate) path: String,
    pub(crate) kind: String,
    #[serde(rename = "abstract")]
    pub(crate) abstract_: String,
    pub(crate) summary: String,
    pub(crate) model: String,
    pub(crate) generated_at: i64,
    pub(crate) children: Vec<SummaryChildResponse>,
    pub(crate) crumbs: Vec<BreadcrumbResponse>,
}

#[derive(Serialize)]
pub(crate) struct ModelInfo {
    pub(crate) name: String,
    pub(crate) size: u64,
}

#[derive(Deserialize)]
pub(crate) struct PullRequest {
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct KeyRequest {
    pub(crate) provider: String,
    pub(crate) key: String,
}

#[derive(Serialize)]
pub(crate) struct KeysStatus {
    pub(crate) openai_set: bool,
    pub(crate) anthropic_set: bool,
    pub(crate) google_set: bool,
}

#[derive(Deserialize)]
pub(crate) struct PathQuery {
    pub(crate) path: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    pub(crate) q: Option<String>,
    pub(crate) limit: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct RootResponse {
    pub(crate) path: String,
    pub(crate) name: String,
}

#[derive(Serialize)]
pub(crate) struct FsEntry {
    pub(crate) name: String,
    pub(crate) path: String,
}

#[derive(Serialize)]
pub(crate) struct QueueStats {
    pub(crate) pending: u64,
    pub(crate) in_flight: u64,
    pub(crate) done: u64,
    pub(crate) failed: u64,
}

#[derive(Serialize)]
pub(crate) struct QueueFailedItem {
    pub(crate) path: String,
    pub(crate) error: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PassesRequest {
    pub(crate) passes_first: u32,
    pub(crate) passes_refresh: u32,
}

#[derive(Serialize)]
pub(crate) struct ConfigResponse {
    pub(crate) passes_first: u32,
    pub(crate) passes_refresh: u32,
    pub(crate) passes_cap: u32,
    pub(crate) max_children_per_summary: usize,
}

#[derive(Deserialize)]
pub(crate) struct AskRequest {
    pub(crate) question: String,
}

#[derive(Serialize)]
pub(crate) struct AskResponse {
    pub(crate) answer: String,
    pub(crate) sources: Vec<AskSource>,
}

#[derive(Serialize)]
pub(crate) struct AskSource {
    pub(crate) path: String,
    pub(crate) heading: String,
    pub(crate) snippet: String,
}

#[derive(Deserialize)]
pub(crate) struct JobPathQuery {
    pub(crate) path: String,
    pub(crate) passes: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct JobStartResponse {
    pub(crate) job_id: Uuid,
}

#[derive(Serialize)]
pub(crate) struct JobListEntry {
    pub(crate) job_id: Uuid,
    pub(crate) kind: String,
    pub(crate) path: String,
    pub(crate) status: JobStatus,
    pub(crate) started_at: i64,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `{"error": msg}` JSON response with the given status.
pub(crate) fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

/// Extract `path` from a `PathQuery`, or return a 400 error response.
/// Accepts an empty string as a valid (present) value — the strictness here
/// mirrors the original handlers' behavior.
#[allow(clippy::result_large_err)] // Response is the natural err type for axum handlers
pub(crate) fn require_path(params: PathQuery) -> Result<String, Response> {
    params
        .path
        .ok_or_else(|| err_json(StatusCode::BAD_REQUEST, "path required"))
}

/// Filename component of a path, falling back to the full path if none.
pub(crate) fn file_name_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

impl From<indexa_core::store::TreeNode> for TreeNodeResponse {
    fn from(n: indexa_core::store::TreeNode) -> Self {
        Self {
            path: n.path,
            name: n.name,
            kind: n.kind,
            child_count: n.child_count,
            byte_size: n.byte_size,
            summary_state: n.summary_state,
            file_count: n.file_count,
            chunk_count: n.chunk_count,
        }
    }
}
