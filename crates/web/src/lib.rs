//! Local web server — axum-based API + embedded HTML/JS UI.
//!
//! Serves at `http://localhost:<port>` with:
//! - `GET /`             — the single-page UI
//! - `GET /api/stats`    — { entries, chunks }
//! - `GET /api/map`      — [{ category, entry_count, total_size }]
//! - `POST /api/ask`     — { question } → { answer, sources }
//! - `POST /api/jobs/index?path=` — start scan→deep→summarize job, returns { job_id }
//! - `GET /api/jobs`     — list active jobs
//! - `GET /api/jobs/:id/events` — SSE progress stream

mod jobs;
use jobs::{broadcast_only, push, JobEvent, JobHandle, JobStatus, Jobs};

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{sse::Event, sse::KeepAlive, sse::Sse, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::StreamExt;
use indexa_core::{
    config::{self, Config},
    store::{ChunkRecord, Store},
    walker::{walk, EntryKind, WalkConfig},
};
use indexa_embed::Embedder;
use indexa_llm::{Generator, OllamaLlm};
use indexa_query::{
    enqueue_subtree, process_queue_item_with_passes, synthesize_from_hits, QaConfig,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;
use uuid::Uuid;

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    store: Arc<Mutex<Store>>,
    embedder: Arc<dyn Embedder + Send + Sync + 'static>,
    llm: Arc<dyn Generator + Send + Sync + 'static>,
    config: Arc<Config>,
    jobs: Jobs,
    db_path: Arc<std::path::PathBuf>,
    log_dir: Arc<std::path::PathBuf>,
    /// Limits concurrent filesystem walks to prevent rayon global-pool starvation.
    walk_semaphore: Arc<tokio::sync::Semaphore>,
}

// ── API types ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatsResponse {
    entries: u64,
    chunks: u64,
}

#[derive(Serialize)]
struct MapRow {
    category: String,
    entry_count: u64,
    total_size: u64,
}

#[derive(Serialize)]
struct TreeNodeResponse {
    path: String,
    name: String,
    kind: String,
    child_count: i64,
    byte_size: i64,
    summary_state: Option<String>,
    file_count: i64,
    chunk_count: i64,
}

#[derive(Serialize)]
struct SummaryChildResponse {
    path: String,
    name: String,
    kind: String,
    summary: String,
    summary_state: Option<String>,
}

#[derive(Serialize)]
struct BreadcrumbResponse {
    path: String,
    name: String,
    summary: String,
}

#[derive(Serialize)]
struct SummaryResponse {
    path: String,
    kind: String,
    summary: String,
    model: String,
    generated_at: i64,
    children: Vec<SummaryChildResponse>,
    crumbs: Vec<BreadcrumbResponse>,
}

#[derive(Serialize)]
struct ModelInfo {
    name: String,
    size: u64,
}

#[derive(Deserialize)]
struct PullRequest {
    name: String,
}

#[derive(Deserialize)]
struct KeyRequest {
    provider: String,
    key: String,
}

#[derive(Serialize)]
struct KeysStatus {
    openai_set: bool,
    anthropic_set: bool,
    google_set: bool,
}

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct RootResponse {
    path: String,
    name: String,
}

#[derive(Serialize)]
struct FsEntry {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct QueueStats {
    pending: u64,
    in_flight: u64,
    done: u64,
    failed: u64,
}

#[derive(Serialize)]
struct QueueFailedItem {
    path: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct PassesRequest {
    passes_first: u32,
    passes_refresh: u32,
}

#[derive(Serialize)]
struct ConfigResponse {
    passes_first: u32,
    passes_refresh: u32,
    passes_cap: u32,
    max_children_per_summary: usize,
}

#[derive(Deserialize)]
struct AskRequest {
    question: String,
}

#[derive(Serialize)]
struct AskResponse {
    answer: String,
    sources: Vec<AskSource>,
}

#[derive(Serialize)]
struct AskSource {
    path: String,
    heading: String,
    snippet: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `{"error": msg}` JSON response with the given status.
fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

/// Extract `path` from a `PathQuery`, or return a 400 error response.
/// Accepts an empty string as a valid (present) value — the strictness here
/// mirrors the original handlers' behavior.
#[allow(clippy::result_large_err)] // Response is the natural err type for axum handlers
fn require_path(params: PathQuery) -> Result<String, Response> {
    params
        .path
        .ok_or_else(|| err_json(StatusCode::BAD_REQUEST, "path required"))
}

/// Filename component of a path, falling back to the full path if none.
fn file_name_of(path: &str) -> String {
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

// ── Route handlers ────────────────────────────────────────────────────────────

async fn api_tree(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Json<Vec<TreeNodeResponse>> {
    let path = params.path.as_deref().unwrap_or("");
    let store = state.store.lock().await;
    let nodes = store.tree_level(path).unwrap_or_default();
    Json(nodes.into_iter().map(TreeNodeResponse::from).collect())
}

async fn api_summary(State(state): State<AppState>, Query(params): Query<PathQuery>) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let store = state.store.lock().await;
    let rec = match store.summary_by_path(&path) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Json(serde_json::json!({"error":"no summary","pending":true})).into_response()
        }
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };

    let children = store.children_summaries(&path).unwrap_or_default();
    let crumbs = store.ancestor_summaries(&path).unwrap_or_default();

    let child_responses: Vec<SummaryChildResponse> = children
        .into_iter()
        .map(|c| SummaryChildResponse {
            name: file_name_of(&c.path),
            path: c.path,
            kind: c.kind,
            summary: c.summary,
            summary_state: Some("done".into()),
        })
        .collect();

    let crumb_responses: Vec<BreadcrumbResponse> = crumbs
        .into_iter()
        .map(|c| BreadcrumbResponse {
            name: file_name_of(&c.path),
            path: c.path,
            summary: c.summary,
        })
        .collect();

    Json(SummaryResponse {
        path: rec.path,
        kind: rec.kind,
        summary: rec.summary,
        model: rec.model,
        generated_at: rec.generated_at,
        children: child_responses,
        crumbs: crumb_responses,
    })
    .into_response()
}

async fn api_summarize_enqueue(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let depth = path.chars().filter(|&c| c == '/' || c == '\\').count() as i64;
    let kind = if std::path::Path::new(&path).is_dir() {
        "dir"
    } else {
        "file"
    };
    let mut store = state.store.lock().await;
    match store.enqueue_summary_items(&[(path.clone(), kind.into(), depth)]) {
        Ok(()) => Json(serde_json::json!({"queued":true,"path":path})).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn api_models_installed(State(state): State<AppState>) -> Response {
    let base = &state.config.describer.base_url;
    let url = format!("{base}/api/tags");
    let resp = match reqwest::Client::new().get(&url).send().await {
        Ok(r) => r,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let models: Vec<ModelInfo> = body["models"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|m| ModelInfo {
            name: m["name"].as_str().unwrap_or("").to_owned(),
            size: m["size"].as_u64().unwrap_or(0),
        })
        .collect();
    Json(models).into_response()
}

async fn api_models_pull(State(state): State<AppState>, Json(body): Json<PullRequest>) -> Response {
    let base = &state.config.describer.base_url;
    let url = format!("{base}/api/pull");
    let resp = match reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({"name": body.name, "stream": true}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    // Proxy the NDJSON stream straight through to the client.
    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(std::io::Error::other));
    Response::builder()
        .status(200)
        .header("Content-Type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap()
        .into_response()
}

async fn api_keys_get(State(state): State<AppState>) -> Json<KeysStatus> {
    let keys = &state.config.api_keys;
    Json(KeysStatus {
        openai_set: keys.openai.as_deref().is_some_and(|k| !k.is_empty()),
        anthropic_set: keys.anthropic.as_deref().is_some_and(|k| !k.is_empty()),
        google_set: keys.google.as_deref().is_some_and(|k| !k.is_empty()),
    })
}

async fn api_keys_set(State(state): State<AppState>, Json(body): Json<KeyRequest>) -> Response {
    // Gate: require env flag to allow writing secrets via the web UI.
    if std::env::var("INDEXA_WEB_ALLOW_KEY_EDIT").as_deref() != Ok("1") {
        return err_json(
            StatusCode::FORBIDDEN,
            "Set INDEXA_WEB_ALLOW_KEY_EDIT=1 to enable API key editing via the web UI.",
        );
    }

    let cfg_path = config::default_config_path();
    let mut cfg = config::load(&cfg_path).unwrap_or_default();

    let key_val = if body.key.is_empty() {
        None
    } else {
        Some(body.key.clone())
    };
    match body.provider.as_str() {
        "openai" => cfg.api_keys.openai = key_val,
        "anthropic" => cfg.api_keys.anthropic = key_val,
        "google" => cfg.api_keys.google = key_val,
        _ => return err_json(StatusCode::BAD_REQUEST, "unknown provider"),
    }

    // Never log key material — log only the provider name.
    let provider = &body.provider;
    let _ = state.config.as_ref(); // keep state referenced
    match config::save(&cfg, &cfg_path) {
        Ok(()) => {
            tracing::info!("API key updated for provider={provider}");
            Json(serde_json::json!({"saved": true, "restart_required": true})).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn api_roots(State(state): State<AppState>) -> Json<Vec<RootResponse>> {
    let store = state.store.lock().await;
    let paths = store.root_paths().unwrap_or_default();
    Json(
        paths
            .into_iter()
            .map(|p| RootResponse {
                name: file_name_of(&p),
                path: p,
            })
            .collect(),
    )
}

async fn api_search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Json<Vec<TreeNodeResponse>> {
    let q = params.q.as_deref().unwrap_or("").trim().to_owned();
    if q.is_empty() {
        return Json(vec![]);
    }
    let limit = params.limit.unwrap_or(50).min(200);
    let store = state.store.lock().await;
    let nodes = store.search_paths(&q, limit).unwrap_or_default();
    Json(nodes.into_iter().map(TreeNodeResponse::from).collect())
}

async fn api_fs_ls(Query(params): Query<PathQuery>) -> Response {
    let raw = match params.path.as_deref() {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => directories::BaseDirs::new()
            .map(|b| b.home_dir().to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_owned()),
    };

    // Security: reject path traversal and non-absolute paths.
    let canon = match std::fs::canonicalize(&raw) {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::NOT_FOUND, "path not found"),
    };

    let home_canon = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .and_then(|h| std::fs::canonicalize(h).ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));

    // Clamp to HOME to prevent exposing system dirs.
    if !canon.starts_with(&home_canon) {
        return err_json(StatusCode::FORBIDDEN, "path outside home directory");
    }

    let mut entries: Vec<FsEntry> = Vec::new();

    // Add parent dir navigation (as long as we're not already at home).
    if canon != home_canon {
        if let Some(parent) = canon.parent() {
            entries.push(FsEntry {
                name: "..".into(),
                path: parent.to_string_lossy().into_owned(),
            });
        }
    }

    let rd = match std::fs::read_dir(&canon) {
        Ok(rd) => rd,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let mut dirs: Vec<FsEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && !e.file_name().to_string_lossy().starts_with('.')
        })
        .map(|e| FsEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path().to_string_lossy().into_owned(),
        })
        .collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    entries.extend(dirs);

    Json(entries).into_response()
}

async fn api_queue_stats(State(state): State<AppState>) -> Json<QueueStats> {
    let store = state.store.lock().await;
    let qs = store.queue_stats().unwrap_or_default();
    Json(QueueStats {
        pending: qs.pending as u64,
        in_flight: qs.in_flight as u64,
        done: qs.done as u64,
        failed: qs.failed as u64,
    })
}

async fn api_queue_failed(State(state): State<AppState>) -> Json<Vec<QueueFailedItem>> {
    let store = state.store.lock().await;
    let items = store.failed_queue_items(50).unwrap_or_default();
    Json(
        items
            .into_iter()
            .map(|i| QueueFailedItem {
                path: i.path,
                error: i.error,
            })
            .collect(),
    )
}

async fn api_queue_retry(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let path = match require_path(params) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut store = state.store.lock().await;
    match store.mark_queue_state(&path, "pending", None) {
        Ok(_) => Json(serde_json::json!({ "queued": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn api_config_get(State(state): State<AppState>) -> Json<ConfigResponse> {
    let cfg = &state.config.describer;
    Json(ConfigResponse {
        passes_first: cfg.passes_first,
        passes_refresh: cfg.passes_refresh,
        passes_cap: cfg.passes_cap,
        max_children_per_summary: cfg.max_children_per_summary,
    })
}

async fn api_config_passes(
    State(state): State<AppState>,
    Json(body): Json<PassesRequest>,
) -> Response {
    if std::env::var("INDEXA_WEB_ALLOW_KEY_EDIT").as_deref() != Ok("1") {
        return err_json(StatusCode::FORBIDDEN, "INDEXA_WEB_ALLOW_KEY_EDIT not set");
    }

    let cap = state.config.describer.passes_cap;
    let first = body.passes_first.min(cap).max(1);
    let refresh = body.passes_refresh.min(cap).max(1);

    let cfg_path = config::default_config_path();
    let mut cfg = config::load(&cfg_path).unwrap_or_default();
    cfg.describer.passes_first = first;
    cfg.describer.passes_refresh = refresh;

    match config::save(&cfg, &cfg_path) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn serve_ui() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_HTML,
    )
        .into_response()
}

async fn serve_ui_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        UI_CSS,
    )
        .into_response()
}

async fn serve_ui_js() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        UI_JS,
    )
        .into_response()
}

async fn api_stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let store = state.store.lock().await;
    let entries = store.entry_count().unwrap_or(0);
    let chunks = store.chunk_count().unwrap_or(0);
    Json(StatsResponse { entries, chunks })
}

async fn api_map(State(state): State<AppState>) -> Json<Vec<MapRow>> {
    let store = state.store.lock().await;
    let rows = store
        .region_summary()
        .unwrap_or_default()
        .into_iter()
        .map(|r| MapRow {
            category: r.category,
            entry_count: r.entry_count,
            total_size: r.total_size,
        })
        .collect();
    Json(rows)
}

async fn api_ask(State(state): State<AppState>, Json(body): Json<AskRequest>) -> Response {
    let qa_cfg = QaConfig {
        top_k: state.config.retrieval.top_k,
        rrf_k: state.config.retrieval.rrf_k as f32,
        summary_weight: state.config.retrieval.summary_weight,
        summary_depth_alpha: state.config.retrieval.summary_depth_alpha,
        ..QaConfig::default()
    };

    // Step 1: embed query (async, no store lock needed).
    let query_vec = match state.embedder.embed(&body.question).await {
        Ok(v) => v,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };

    // Step 2: sync store query (hold lock only for the synchronous call, no await).
    let hits = {
        let store = state.store.lock().await;
        let mut hits = match store.hybrid_search(
            &body.question,
            Some(&query_vec),
            &indexa_core::config::HybridMode::Rrf,
            None,
            qa_cfg.top_k,
            qa_cfg.rrf_k,
        ) {
            Ok(h) => h,
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
        // Optional summary-boost reranking (no-op when summary_weight == 0.0).
        let _ = store.boost_with_summaries(
            &mut hits,
            &query_vec,
            qa_cfg.summary_weight,
            qa_cfg.summary_depth_alpha,
        );
        hits
    }; // MutexGuard dropped here — no store reference held across awaits

    // Step 3: LLM synthesis (async, store lock already released).
    match synthesize_from_hits(hits, state.llm.as_ref(), &body.question, &qa_cfg).await {
        Ok(answer) => Json(AskResponse {
            answer: answer.answer,
            sources: answer
                .sources
                .into_iter()
                .map(|s| AskSource {
                    path: s.path,
                    heading: s.heading,
                    snippet: s.snippet,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

// ── Background job API ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct JobPathQuery {
    path: String,
    passes: Option<u32>,
}

#[derive(Serialize)]
struct JobStartResponse {
    job_id: Uuid,
}

#[derive(Serialize)]
struct JobListEntry {
    job_id: Uuid,
    kind: String,
    path: String,
    status: JobStatus,
    started_at: i64,
}

/// Register a new job in the shared registry and return its handle + id.
async fn register_job(jobs: &Jobs, kind: &str, path: String) -> (Uuid, Arc<JobHandle>) {
    let handle = Arc::new(JobHandle::new(kind, path));
    let id = handle.id;
    jobs.write().await.insert(id, handle.clone());
    (id, handle)
}

async fn api_job_scan(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (id, handle) = register_job(&s.jobs, "scan", q.path.clone()).await;
    let state = s.clone();
    tokio::spawn(async move {
        run_scan_phase_standalone(&state, &q.path, &handle).await;
        schedule_cleanup(state.jobs.clone(), handle.id);
    });
    Json(JobStartResponse { job_id: id })
}

async fn api_job_deep(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (id, handle) = register_job(&s.jobs, "deep", q.path.clone()).await;
    let state = s.clone();
    tokio::spawn(async move {
        run_deep_phase_standalone(&state, &q.path, &handle).await;
        schedule_cleanup(state.jobs.clone(), handle.id);
    });
    Json(JobStartResponse { job_id: id })
}

async fn api_job_summarize(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (id, handle) = register_job(&s.jobs, "summarize", q.path.clone()).await;
    let state = s.clone();
    tokio::spawn(async move {
        run_summarize_phase(&state, &q.path, q.passes, &handle).await;
        schedule_cleanup(state.jobs.clone(), handle.id);
    });
    Json(JobStartResponse { job_id: id })
}

async fn api_job_index(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (id, handle) = register_job(&s.jobs, "index", q.path.clone()).await;
    let state = s.clone();
    tokio::spawn(async move {
        let id = handle.id;
        run_index_job(state.clone(), q.path, handle).await;
        schedule_cleanup(state.jobs.clone(), id);
    });
    Json(JobStartResponse { job_id: id })
}

async fn api_jobs_list(State(s): State<AppState>) -> impl IntoResponse {
    let jobs = s.jobs.read().await;
    let list: Vec<JobListEntry> = jobs
        .values()
        .map(|h| JobListEntry {
            job_id: h.id,
            kind: h.kind.clone(),
            path: h.path.clone(),
            status: h.status.lock().unwrap().clone(),
            started_at: h.started_at,
        })
        .collect();
    Json(list)
}

async fn api_jobs_events(Path(id): Path<Uuid>, State(s): State<AppState>) -> impl IntoResponse {
    let handle = match s.jobs.read().await.get(&id) {
        Some(h) => h.clone(),
        None => return (StatusCode::NOT_FOUND, "job not found").into_response(),
    };

    let history = handle.history.lock().unwrap().clone();
    let rx = handle.tx.subscribe();

    fn to_sse(ev: JobEvent) -> Result<Event, Infallible> {
        let data = serde_json::to_string(&ev).unwrap_or_default();
        Ok(Event::default().data(data))
    }

    let replay = futures_util::stream::iter(history).map(to_sse);
    let live = BroadcastStream::new(rx)
        .map(|r| match r {
            Ok(ev) => ev,
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                JobEvent::Warning {
                    stage: "sse".into(),
                    item_path: None,
                    message: format!("dropped {n} events — refresh to resync"),
                }
            }
        })
        .map(to_sse);

    Sse::new(replay.chain(live))
        .keep_alive(KeepAlive::new())
        .into_response()
}

/// JSON snapshot of a single job (status + last progress event) without SSE.
async fn api_job_get(Path(id): Path<Uuid>, State(s): State<AppState>) -> impl IntoResponse {
    let jobs = s.jobs.read().await;
    let Some(h) = jobs.get(&id) else {
        return err_json(StatusCode::NOT_FOUND, "job not found");
    };
    let status = h.status.lock().unwrap().clone();
    let last_event = h.history.lock().unwrap().last().cloned();
    let resp = serde_json::json!({
        "job_id": h.id,
        "kind": h.kind,
        "path": h.path,
        "started_at": h.started_at,
        "status": status,
        "last_event": last_event,
    });
    (StatusCode::OK, Json(resp)).into_response()
}

async fn api_job_delete(Path(id): Path<Uuid>, State(s): State<AppState>) -> impl IntoResponse {
    let mut jobs = s.jobs.write().await;
    if let Some(handle) = jobs.get(&id) {
        *handle.status.lock().unwrap() = JobStatus::Failed;
    }
    jobs.remove(&id);
    StatusCode::NO_CONTENT
}

// ── Entry management ──────────────────────────────────────────────────────────

async fn api_delete_entry(
    Query(q): Query<PathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let path = match require_path(q) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut store = s.store.lock().await;
    match store.delete_subtree(&path) {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn api_version() -> impl IntoResponse {
    Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }))
}

/// Return the last N lines of today's log file (for error reports).
async fn api_logs_tail(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let lines: usize = params
        .get("lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(500);

    // tracing-appender rolling::daily creates files named "prefix.YYYY-MM-DD".
    // Pick the most recently modified log file under the log dir.
    let log_dir = &*state.log_dir;
    let candidates: Vec<_> = std::fs::read_dir(log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("indexa.log"))
        .collect();

    // Pick the most recently modified log file.
    let best = candidates
        .iter()
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    let content = match best {
        Some(entry) => std::fs::read_to_string(entry.path()).unwrap_or_default(),
        None => String::new(),
    };

    let tail: String = content
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    Json(serde_json::json!({ "lines": tail }))
}

// ── Job runner ────────────────────────────────────────────────────────────────

/// Schedule removal of a job from the registry after 60 s. Allows refreshed
/// clients to re-subscribe to recently-finished jobs and replay history.
fn schedule_cleanup(jobs: Jobs, id: uuid::Uuid) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        jobs.write().await.remove(&id);
    });
}

fn finalize_failed(handle: &Arc<JobHandle>, stage: &str, err: &anyhow::Error) {
    let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
    let error = format!("{err:#}");
    push(
        handle,
        JobEvent::Failed {
            error,
            stage: Some(stage.to_owned()),
            item_path: None,
            chain: if chain.len() > 1 { Some(chain) } else { None },
            code: None,
        },
    );
    *handle.status.lock().unwrap() = JobStatus::Failed;
}

fn finalize_done(handle: &Arc<JobHandle>, summary: &str) {
    push(
        handle,
        JobEvent::Done {
            summary: summary.to_owned(),
        },
    );
    *handle.status.lock().unwrap() = JobStatus::Done;
}

/// Walk a path in a blocking thread; on failure, push the error to the job and return None.
/// Acquires a permit from `sem` to limit concurrent walks and prevent rayon pool starvation.
async fn walk_for_job(
    path: &str,
    handle: &Arc<JobHandle>,
    sem: &tokio::sync::Semaphore,
) -> Option<Vec<indexa_core::walker::Entry>> {
    let _permit = sem.acquire().await.ok()?;
    let pb = std::path::PathBuf::from(path);
    let walked = tokio::task::spawn_blocking(move || walk(&pb, &WalkConfig::default()))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!(e)));
    match walked {
        Ok(e) => Some(e),
        Err(e) => {
            finalize_failed(handle, "walk", &e);
            None
        }
    }
}

async fn run_index_job(state: AppState, path: String, handle: Arc<JobHandle>) {
    // Phase 1: scan
    let Some(entries) = walk_for_job(&path, &handle, &state.walk_semaphore).await else {
        return;
    };

    if !run_scan_phase_with_entries(&state, &path, &entries, &handle).await {
        return;
    }

    // Phase 2: deep index
    if !run_deep_phase(&state, &path, &entries, &handle).await {
        return;
    }

    // Phase 3: summarize
    run_summarize_phase(&state, &path, None, &handle).await;
}

/// Standalone scan: walks, scans, then finalises the job as done.
async fn run_scan_phase_standalone(state: &AppState, path: &str, handle: &Arc<JobHandle>) {
    let Some(entries) = walk_for_job(path, handle, &state.walk_semaphore).await else {
        return;
    };
    if run_scan_phase_with_entries(state, path, &entries, handle).await {
        let n = entries.len() as u64;
        finalize_done(handle, &format!("{n} entries scanned"));
    }
}

/// Standalone deep: walks, deep-indexes, then finalises the job as done.
async fn run_deep_phase_standalone(state: &AppState, path: &str, handle: &Arc<JobHandle>) {
    let Some(entries) = walk_for_job(path, handle, &state.walk_semaphore).await else {
        return;
    };
    let n_files = entries.iter().filter(|e| e.kind == EntryKind::File).count();
    if run_deep_phase(state, path, &entries, handle).await {
        finalize_done(handle, &format!("Deep index complete: {n_files} files"));
    }
}

async fn run_scan_phase_with_entries(
    state: &AppState,
    path: &str,
    entries: &[indexa_core::walker::Entry],
    handle: &Arc<JobHandle>,
) -> bool {
    let n = entries.len() as u64;
    push(
        handle,
        JobEvent::Start {
            kind: "scan".into(),
            path: path.to_owned(),
            total: Some(n),
        },
    );

    let live_paths: std::collections::HashSet<String> = entries
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();

    let mut store = state.store.lock().await;
    if let Err(e) = store.upsert_entries(entries) {
        finalize_failed(handle, "scan", &e);
        return false;
    }
    if let Err(e) = store.reconcile_entries(path, &live_paths) {
        push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("{e:#}"),
            },
        );
    }
    drop(store);

    push(
        handle,
        JobEvent::Progress {
            current: n,
            total: n,
            note: Some(format!("{n} entries scanned")),
            current_path: None,
            items_per_sec: None,
            eta_secs: None,
        },
    );
    true
}

/// Returns true on success.
async fn run_deep_phase(
    state: &AppState,
    path: &str,
    entries: &[indexa_core::walker::Entry],
    handle: &Arc<JobHandle>,
) -> bool {
    let files: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .collect();
    let n_files = files.len() as u64;
    let total_bytes: u64 = files.iter().map(|e| e.size).sum();

    push(
        handle,
        JobEvent::Start {
            kind: "deep".into(),
            path: path.to_owned(),
            total: Some(n_files),
        },
    );
    push(
        handle,
        JobEvent::Snapshot {
            count: n_files,
            bytes: total_bytes,
        },
    );

    let embed_model = state.config.embedding.model.clone();
    let cfg = state.config.describer.clone();
    // Build a contextual-retrieval LLM if the feature is enabled.
    let ctx_llm: Option<OllamaLlm> = if cfg.contextual_retrieval {
        let base_url = OllamaLlm::resolve_base_url(Some(&cfg.base_url));
        Some(OllamaLlm::new(&base_url, &cfg.file_model))
    } else {
        None
    };

    let mut done = 0u64;
    // Rolling throughput: ring buffer of (instant, items_done) samples, last ~5s.
    let mut samples: std::collections::VecDeque<(std::time::Instant, u64)> =
        std::collections::VecDeque::with_capacity(16);
    samples.push_back((std::time::Instant::now(), 0));

    for entry in &files {
        let path_str = entry.path.to_string_lossy().into_owned();

        let is_current = {
            let store = state.store.lock().await;
            store.chunks_are_current(&path_str).unwrap_or(false)
        };
        if is_current {
            done += 1;
        } else {
            let ep = entry.path.clone();
            let extracted =
                match tokio::task::spawn_blocking(move || indexa_parsers::registry::parse(&ep))
                    .await
                {
                    Ok(Ok(e)) => e,
                    Ok(Err(e)) => {
                        push(
                            handle,
                            JobEvent::Warning {
                                stage: "deep".to_owned(),
                                item_path: Some(path_str.clone()),
                                message: format!("{e:#}"),
                            },
                        );
                        done += 1;
                        continue;
                    }
                    Err(e) => {
                        push(
                            handle,
                            JobEvent::Warning {
                                stage: "deep".to_owned(),
                                item_path: Some(path_str.clone()),
                                message: format!("parse task panicked: {e}"),
                            },
                        );
                        done += 1;
                        continue;
                    }
                };

            if !extracted.chunks.is_empty() {
                // Build a document-level context string for contextual retrieval.
                let doc_context: Option<String> = ctx_llm.as_ref().map(|_| {
                    let joined: String = extracted
                        .chunks
                        .iter()
                        .map(|c| c.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    joined.chars().take(4000).collect()
                });

                let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
                for chunk in &extracted.chunks {
                    // Optionally prepend a context blurb generated by the file LLM.
                    let embed_text =
                        if let (Some(ref llm), Some(ref doc)) = (&ctx_llm, &doc_context) {
                            let prompt = format!(
                                "<document>\n{doc}\n</document>\n\n\
                             Here is the chunk we want to situate within the whole document:\n\
                             <chunk>\n{}\n</chunk>\n\n\
                             Give a short succinct context (1-2 sentences) to situate this chunk \
                             within the overall document for improved search retrieval. \
                             Answer only with the succinct context and nothing else.",
                                chunk.text
                            );
                            let ps = path_str.clone();
                            let model_name = cfg.file_model.clone();
                            let h = handle.clone();
                            let mut on_frag = move |frag: String| {
                                broadcast_only(
                                    &h,
                                    JobEvent::LlmFragment {
                                        item_path: ps.clone(),
                                        model: model_name.clone(),
                                        stage: "context_blurb".to_owned(),
                                        fragment: frag,
                                    },
                                );
                            };
                            match llm.generate_stream(&prompt, &mut on_frag).await {
                                Ok(blurb) => format!("{}\n\n{}", blurb.trim(), chunk.text),
                                Err(e) => {
                                    push(
                                        handle,
                                        JobEvent::Warning {
                                            stage: "deep".to_owned(),
                                            item_path: Some(path_str.clone()),
                                            message: format!("context blurb failed: {e:#}"),
                                        },
                                    );
                                    chunk.text.clone()
                                }
                            }
                        } else {
                            chunk.text.clone()
                        };

                    let embedding = match state.embedder.embed(&embed_text).await {
                        Ok(v) => Some(v),
                        Err(e) => {
                            push(
                                handle,
                                JobEvent::Warning {
                                    stage: "deep".to_owned(),
                                    item_path: Some(path_str.clone()),
                                    message: format!("embed failed: {e:#}"),
                                },
                            );
                            None
                        }
                    };
                    chunk_records.push(ChunkRecord {
                        entry_path: path_str.clone(),
                        seq: chunk.seq,
                        heading: chunk.heading.clone(),
                        text: chunk.text.clone(), // store original text, embed enriched
                        language: chunk.language.clone(),
                        embedding,
                        embed_model: Some(embed_model.clone()),
                    });
                }
                let mut store = state.store.lock().await;
                if let Err(e) = store.upsert_chunks(&chunk_records) {
                    push(
                        handle,
                        JobEvent::Warning {
                            stage: "deep".to_owned(),
                            item_path: Some(path_str.clone()),
                            message: format!("upsert_chunks failed: {e:#}"),
                        },
                    );
                }
            }
            done += 1;
        }

        // Update rolling throughput window (evict samples older than 5s).
        let now = std::time::Instant::now();
        let cutoff = now - std::time::Duration::from_secs(5);
        while samples.len() > 1 && samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            samples.pop_front();
        }
        samples.push_back((now, done));

        let (rate, eta) = if samples.len() >= 2 {
            let (oldest_t, oldest_done) = samples.front().unwrap();
            let elapsed = oldest_t.elapsed().as_secs_f64();
            let r = if elapsed > 0.0 {
                (done - oldest_done) as f64 / elapsed
            } else {
                0.0
            };
            let e = if r > 0.0 {
                (n_files - done) as f64 / r
            } else {
                0.0
            };
            (Some(r), Some(e))
        } else {
            (None, None)
        };

        push(
            handle,
            JobEvent::Progress {
                current: done,
                total: n_files,
                note: None,
                current_path: Some(path_str),
                items_per_sec: rate,
                eta_secs: eta,
            },
        );
    }

    true
}

async fn run_summarize_phase(
    state: &AppState,
    path: &str,
    passes_override: Option<u32>,
    handle: &Arc<JobHandle>,
) {
    push(
        handle,
        JobEvent::Start {
            kind: "summarize".into(),
            path: path.to_owned(),
            total: None,
        },
    );

    let db_path = (*state.db_path).clone();
    let cfg = state.config.describer.clone();
    let embedder = state.embedder.clone();
    let root = std::path::PathBuf::from(path);
    let base_url = OllamaLlm::resolve_base_url(Some(&cfg.base_url));
    let describer = OllamaLlm::new_with_dir_model(&base_url, &cfg.file_model, &cfg.dir_model);

    // Open a dedicated Store connection so we can hold it across async LLM awaits
    // without poisoning the shared mutex-wrapped store used by API handlers.
    let mut job_store = match indexa_core::store::Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    let enqueued = match enqueue_subtree(&mut job_store, &root) {
        Ok(n) => n,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    push(
        handle,
        JobEvent::Snapshot {
            count: enqueued as u64,
            bytes: 0,
        },
    );

    let mut done = 0usize;
    let mut errors = 0usize;
    let mut samples: std::collections::VecDeque<(std::time::Instant, u64)> =
        std::collections::VecDeque::with_capacity(16);
    samples.push_back((std::time::Instant::now(), 0));

    loop {
        let item = match job_store.next_queue_item() {
            Ok(Some(i)) => i,
            Ok(None) => break,
            Err(e) => {
                finalize_failed(handle, "summarize", &e);
                return;
            }
        };
        let item_path = item.path.clone();
        let llm_start = std::time::Instant::now();
        let r = process_queue_item_with_passes(
            &mut job_store,
            &describer,
            embedder.as_ref(),
            &item,
            &cfg,
            passes_override,
        )
        .await;
        let llm_secs = llm_start.elapsed().as_secs_f64();
        match r {
            Ok(()) => done += 1,
            Err(_) => errors += 1,
        }

        let processed = (done + errors) as u64;
        let now = std::time::Instant::now();
        let cutoff = now - std::time::Duration::from_secs(5);
        while samples.len() > 1 && samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            samples.pop_front();
        }
        samples.push_back((now, processed));

        let (rate, eta) = if samples.len() >= 2 {
            let (oldest_t, oldest_done) = samples.front().unwrap();
            let elapsed = oldest_t.elapsed().as_secs_f64();
            let r = if elapsed > 0.0 {
                (processed - oldest_done) as f64 / elapsed
            } else {
                0.0
            };
            let e = if r > 0.0 {
                (enqueued as u64 - processed) as f64 / r
            } else {
                0.0
            };
            (Some(r), Some(e))
        } else {
            (None, None)
        };

        let note = Some(format!("{:.1}s · {}", llm_secs, cfg.file_model));
        push(
            handle,
            JobEvent::Progress {
                current: processed,
                total: enqueued as u64,
                note,
                current_path: Some(item_path),
                items_per_sec: rate,
                eta_secs: eta,
            },
        );
    }

    push(
        handle,
        JobEvent::Done {
            summary: format!("{done} summaries generated"),
        },
    );
    *handle.status.lock().unwrap() = JobStatus::Done;
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Start the web UI server on `port`. Runs until Ctrl-C or the process exits.
pub async fn serve(
    port: u16,
    store: Store,
    embedder: Arc<dyn Embedder + Send + Sync + 'static>,
    llm: Arc<dyn Generator + Send + Sync + 'static>,
    config: Config,
) -> Result<()> {
    let db_path = Arc::new(store.db_path().to_path_buf());
    let log_dir = Arc::new(
        indexa_core::config::default_data_dir()
            .map(|d| d.join("logs"))
            .unwrap_or_else(|| std::env::temp_dir().join("indexa-logs")),
    );
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        embedder,
        llm,
        config: Arc::new(config),
        jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        db_path,
        log_dir,
        walk_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
    };

    // Restrict CORS to localhost only — prevents drive-by sites from reading the
    // user's private index via cross-origin requests to the local server.
    let origin = format!("http://localhost:{port}")
        .parse::<axum::http::HeaderValue>()
        .expect("valid localhost origin header");

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/assets/app.css", get(serve_ui_css))
        .route("/assets/app.js", get(serve_ui_js))
        .route("/api/stats", get(api_stats))
        .route("/api/map", get(api_map))
        .route("/api/roots", get(api_roots))
        .route("/api/search", get(api_search))
        .route("/api/fs/ls", get(api_fs_ls))
        .route("/api/ask", post(api_ask))
        .route("/api/tree", get(api_tree))
        .route("/api/summary", get(api_summary))
        .route("/api/summarize", post(api_summarize_enqueue))
        .route("/api/queue", get(api_queue_stats))
        .route("/api/queue/failed", get(api_queue_failed))
        .route("/api/queue/retry", post(api_queue_retry))
        .route("/api/config", get(api_config_get))
        .route("/api/config/passes", post(api_config_passes))
        .route("/api/models/installed", get(api_models_installed))
        .route("/api/models/pull", post(api_models_pull))
        .route("/api/keys", get(api_keys_get).post(api_keys_set))
        .route("/api/jobs", get(api_jobs_list))
        .route("/api/jobs/scan", post(api_job_scan))
        .route("/api/jobs/deep", post(api_job_deep))
        .route("/api/jobs/summarize", post(api_job_summarize))
        .route("/api/jobs/index", post(api_job_index))
        .route("/api/jobs/:id/events", get(api_jobs_events))
        .route("/api/jobs/:id", get(api_job_get).delete(api_job_delete))
        .route("/api/entry", delete(api_delete_entry))
        .route("/api/version", get(api_version))
        .route("/api/logs/tail", get(api_logs_tail))
        .with_state(state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::DELETE,
                ])
                .allow_headers([header::CONTENT_TYPE]),
        );

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Indexa web UI listening on http://{addr}");
    println!("Open http://localhost:{port} in your browser. Press Ctrl-C to stop.");

    axum::serve(listener, app).await?;
    Ok(())
}

// ── Embedded UI (split into asset files, included at compile time) ──────────

const UI_HTML: &str = include_str!("../assets/ui/index.html");
const UI_CSS: &str = include_str!("../assets/ui/app.css");
const UI_JS: &str = include_str!("../assets/ui/app.js");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_assets_non_empty() {
        assert!(!UI_HTML.is_empty());
        assert!(UI_HTML.contains("Indexa"));
        assert!(!UI_CSS.is_empty());
        assert!(!UI_JS.is_empty());
        assert!(UI_JS.contains("/api/ask"));
    }
}
