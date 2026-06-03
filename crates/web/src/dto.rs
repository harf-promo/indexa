//! Request/response DTOs, small helpers, and conversions shared by the handlers.

use crate::jobs::JobStatus;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::resource::{assess, compute_budget, CpuSample, MachineSpec, MemSample, Pressure};
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

/// One node in the treemap tree (nested; children omitted when empty).
#[derive(Serialize)]
pub(crate) struct TreemapNodeDto {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) file_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) children: Vec<TreemapNodeDto>,
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
    /// Subtree context-coverage rollup (directory summaries). `total` = dirs in the subtree,
    /// `covered` = built (`done`), `partial` = queued. All 0 for file nodes.
    pub(crate) covered: i64,
    pub(crate) partial: i64,
    pub(crate) total: i64,
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

/// Query for `GET /api/models`. `num_ctx` overrides the configured context
/// window for the peak/ETA estimate; `?path=` scoping is deferred to PR-C.
#[derive(Deserialize)]
pub(crate) struct ModelsQuery {
    pub(crate) num_ctx: Option<u32>,
}

/// One model in the unified `GET /api/models` list — installed or catalog-only.
#[derive(Serialize)]
pub(crate) struct ModelRow {
    pub(crate) name: String,
    /// `embed` | `file` | `dir` | `qa` | `code` | `vision`.
    pub(crate) role: String,
    pub(crate) vendor: String,
    pub(crate) params_b: Option<f64>,
    /// Real on-disk size when installed, else the estimated weights size.
    pub(crate) size_bytes: u64,
    pub(crate) size_is_estimate: bool,
    pub(crate) installed: bool,
    /// Peak resident bytes (weights + KV) at the requested `num_ctx`.
    pub(crate) peak_bytes: u64,
    /// Whether `peak_bytes` fits the live memory budget.
    pub(crate) fits: bool,
    pub(crate) eta_display: String,
    pub(crate) eta_secs: u64,
    pub(crate) recommended_default: bool,
    pub(crate) safe_default: bool,
}

#[derive(Serialize)]
pub(crate) struct ModelsResponse {
    pub(crate) budget_bytes: i64,
    pub(crate) num_ctx: u32,
    pub(crate) models: Vec<ModelRow>,
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

/// Status of the `claude-code` (Claude subscription) provider, surfaced in the
/// Settings UI and `doctor`. All fields come from token-free local probes; `email`
/// from `claude auth status` is deliberately NOT forwarded (PII, no UI need).
#[derive(Serialize)]
pub(crate) struct ProviderStatus {
    /// The active `[describer] provider` (e.g. `"ollama"` or `"claude-code"`).
    pub(crate) describer_provider: String,
    /// `claude` CLI resolved and responded to `--version`.
    pub(crate) claude_cli_present: bool,
    /// Version string from `claude --version` (e.g. `"2.1.158"`).
    pub(crate) claude_cli_version: Option<String>,
    /// `claude auth status` reports a logged-in subscription session.
    pub(crate) claude_logged_in: bool,
    /// Auth method, e.g. `"claude.ai"`.
    pub(crate) claude_auth_method: Option<String>,
    /// Subscription tier, e.g. `"max"` / `"pro"`.
    pub(crate) claude_subscription: Option<String>,
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
    /// Active describer provider (`"ollama"` | `"claude-code"` | …).
    pub(crate) describer_provider: String,
    /// Ollama base URL (the IP/self-hosted endpoint).
    pub(crate) base_url: String,
    /// Active model per role — lets the Settings UI mark the live rows.
    pub(crate) file_model: String,
    pub(crate) dir_model: String,
    pub(crate) qa_model: String,
    pub(crate) embed_model: String,
}

/// Inbound model/provider assignment from the Settings UI. Every field is
/// optional; only the present ones are written. Gated like `api_config_passes`.
#[derive(Deserialize)]
pub(crate) struct ProviderRequest {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) file_model: Option<String>,
    pub(crate) dir_model: Option<String>,
    pub(crate) embed_model: Option<String>,
    pub(crate) base_url: Option<String>,
}

/// Current resource/workload settings returned to the Settings UI.
/// `profile` is the lowercase enum name ("conservative" | "balanced" | "performance").
#[derive(Serialize)]
pub(crate) struct ResourceResponse {
    pub(crate) profile: String,
    pub(crate) headroom_gb: f32,
}

/// Inbound resource/workload update from the Settings UI.
/// Deliberately holds ONLY profile + headroom — no key material, no other config
/// sections — so the (ungated) write path cannot touch secrets. See
/// `api_config_resource_set` for the full rationale.
#[derive(Deserialize)]
pub(crate) struct ResourceRequest {
    pub(crate) profile: String,
    pub(crate) headroom_gb: f32,
}

/// Advanced opt-in feature toggles exposed in the Settings → Advanced drawer section.
/// Ungated (no INDEXA_WEB_ALLOW_KEY_EDIT) — same rationale as ResourceRequest: no
/// secrets here. Changes apply to the next `indexa deep` run (restart_required: true).
#[derive(Serialize)]
pub(crate) struct FeaturesResponse {
    pub(crate) ann: bool,
    pub(crate) ann_min_chunks: usize,
    pub(crate) image_caption: bool,
    pub(crate) image_model: Option<String>,
    pub(crate) audio_transcribe: bool,
    pub(crate) audio_binary: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct FeaturesRequest {
    pub(crate) ann: Option<bool>,
    pub(crate) ann_min_chunks: Option<usize>,
    pub(crate) image_caption: Option<bool>,
    pub(crate) image_model: Option<String>,
    pub(crate) audio_transcribe: Option<bool>,
    pub(crate) audio_binary: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AskRequest {
    pub(crate) question: String,
}

#[derive(Deserialize)]
pub(crate) struct UpdateRequest {
    /// Optional specific release tag to install, e.g. `"v0.12.1"`.
    /// When absent the latest release is used.
    pub(crate) pin: Option<String>,
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
    /// Optional summarization model override (the "ask me first" choice). When
    /// `dir_model` is set, the job loads these instead of the configured models.
    pub(crate) file_model: Option<String>,
    pub(crate) dir_model: Option<String>,
    pub(crate) num_ctx: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct JobStartResponse {
    pub(crate) job_id: Uuid,
}

/// Pre-flight memory-fit estimate for a summarize/build job — the data behind the
/// "ask me first" popover. Mapped by hand from `resource::fit_report` (whose core
/// types stay non-Serialize). Counts are global, so the ETA is approximate.
#[derive(Serialize)]
pub(crate) struct EstimateResponse {
    pub(crate) budget_bytes: i64,
    pub(crate) configured_file_model: String,
    pub(crate) configured_dir_model: String,
    pub(crate) configured_peak_bytes: u64,
    pub(crate) configured_fits: bool,
    pub(crate) recommended_file_model: Option<String>,
    pub(crate) recommended_dir_model: Option<String>,
    pub(crate) recommended_peak_bytes: Option<u64>,
    pub(crate) recommended_fits: Option<bool>,
    pub(crate) num_ctx: u32,
    pub(crate) reason: Option<String>,
    pub(crate) eta_display: String,
    pub(crate) eta_secs: u64,
    /// Approximate — these counts are global, not scoped to `path`.
    pub(crate) entry_count: u64,
    pub(crate) chunk_count: u64,
    pub(crate) queue_pending: i64,
}

#[derive(Serialize)]
pub(crate) struct JobListEntry {
    pub(crate) job_id: Uuid,
    pub(crate) kind: String,
    pub(crate) path: String,
    pub(crate) status: JobStatus,
    pub(crate) started_at: i64,
}

// ── Machine telemetry (always-on resource feed) ────────────────────────────────
//
// Powers the Engine status bar's live CPU/RAM/pressure gauges. Sampled on its own
// low-frequency task (see `serve()`), independent of any job, so the gauges are
// live even when idle. `pressure`/`budget_bytes`/`in_headroom_band` are derived
// with the SAME core functions the watchdog uses, so the gauge and the watchdog
// always agree on "do we have room".

#[derive(Serialize, Clone)]
pub(crate) struct CpuDto {
    pub(crate) global_percent: f32,
    pub(crate) per_core: Vec<f32>,
}

#[derive(Serialize, Clone, Default)]
pub(crate) struct RamDto {
    pub(crate) total_bytes: u64,
    pub(crate) used_bytes: u64,
    pub(crate) used_percent: f32,
}

#[derive(Serialize, Clone, Default)]
pub(crate) struct SwapDto {
    pub(crate) used_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) used_percent: f32,
}

#[derive(Serialize, Clone, Default)]
pub(crate) struct MachineDto {
    pub(crate) total_ram_bytes: u64,
    pub(crate) physical_cores: usize,
    pub(crate) logical_cores: usize,
    pub(crate) is_apple_silicon: bool,
    pub(crate) gpu_wired_limit_bytes: u64,
}

#[derive(Serialize, Clone)]
pub(crate) struct ActiveJobDto {
    pub(crate) job_id: Uuid,
    pub(crate) kind: String,
    pub(crate) path: String,
}

/// One snapshot of machine telemetry, broadcast over `/api/telemetry`.
#[derive(Serialize, Clone, Default)]
pub(crate) struct TelemetrySample {
    /// Unix seconds when sampled.
    pub(crate) ts: u64,
    /// `None` on the first tick (CPU usage needs two refreshes to prime).
    pub(crate) cpu: Option<CpuDto>,
    pub(crate) ram: RamDto,
    pub(crate) swap: SwapDto,
    /// "ok" | "throttle" | "critical" — from `assess()`.
    pub(crate) pressure: String,
    /// `compute_budget()`: free RAM for a new model load, minus headroom.
    pub(crate) budget_bytes: i64,
    pub(crate) headroom_bytes: u64,
    /// `budget_bytes <= 0` — the RAM bar has entered the keep-free band.
    pub(crate) in_headroom_band: bool,
    pub(crate) machine: MachineDto,
    /// The currently-running job, if any (lets the bar say "watching while building").
    pub(crate) active_job: Option<ActiveJobDto>,
}

impl TelemetrySample {
    /// Compose a sample from a CPU+memory reading plus machine/job context.
    /// Reuses `compute_budget`/`assess` so the gauge matches the watchdog exactly.
    pub(crate) fn build(
        spec: &MachineSpec,
        mem: &MemSample,
        cpu: Option<CpuSample>,
        headroom_bytes: u64,
        active_job: Option<ActiveJobDto>,
        ts: u64,
    ) -> Self {
        let budget_bytes = compute_budget(spec, mem, headroom_bytes);
        let pressure = match assess(mem, spec, headroom_bytes) {
            Pressure::Ok => "ok",
            Pressure::Throttle => "throttle",
            Pressure::Critical => "critical",
        };
        let pct = |num: u64, den: u64| -> f32 {
            if den > 0 {
                (num as f64 / den as f64 * 100.0) as f32
            } else {
                0.0
            }
        };
        Self {
            ts,
            cpu: cpu.map(|c| CpuDto {
                global_percent: c.global_percent,
                per_core: c.per_core,
            }),
            ram: RamDto {
                total_bytes: spec.total_ram_bytes,
                used_bytes: mem.used_bytes,
                used_percent: pct(mem.used_bytes, spec.total_ram_bytes),
            },
            swap: SwapDto {
                used_bytes: mem.swap_used_bytes,
                total_bytes: mem.swap_total_bytes,
                used_percent: pct(mem.swap_used_bytes, mem.swap_total_bytes),
            },
            pressure: pressure.to_owned(),
            budget_bytes,
            headroom_bytes,
            in_headroom_band: budget_bytes <= 0,
            machine: MachineDto {
                total_ram_bytes: spec.total_ram_bytes,
                physical_cores: spec.physical_cores,
                logical_cores: spec.logical_cores,
                is_apple_silicon: spec.is_apple_silicon,
                gpu_wired_limit_bytes: spec.gpu_wired_limit_bytes,
            },
            active_job,
        }
    }
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
            covered: n.covered,
            partial: n.partial,
            total: n.total,
        }
    }
}
