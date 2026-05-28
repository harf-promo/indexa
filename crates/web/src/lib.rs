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
use jobs::{push, JobEvent, JobHandle, JobStatus, Jobs};

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

// ── Route handlers ────────────────────────────────────────────────────────────

async fn api_tree(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Json<Vec<TreeNodeResponse>> {
    let path = params.path.as_deref().unwrap_or("");
    let store = state.store.lock().await;
    let nodes = store.tree_level(path).unwrap_or_default();
    Json(
        nodes
            .into_iter()
            .map(|n| TreeNodeResponse {
                path: n.path,
                name: n.name,
                kind: n.kind,
                child_count: n.child_count,
                byte_size: n.byte_size,
                summary_state: n.summary_state,
            })
            .collect(),
    )
}

async fn api_summary(State(state): State<AppState>, Query(params): Query<PathQuery>) -> Response {
    let path = match params.path {
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"path required"})),
            )
                .into_response()
        }
        Some(p) => p,
    };

    let store = state.store.lock().await;
    let rec = match store.summary_by_path(&path) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Json(serde_json::json!({"error":"no summary","pending":true})).into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":e.to_string()})),
            )
                .into_response()
        }
    };

    let children = store.children_summaries(&path).unwrap_or_default();
    let crumbs = store.ancestor_summaries(&path).unwrap_or_default();

    let child_responses: Vec<SummaryChildResponse> = children
        .iter()
        .map(|c| {
            let name = std::path::Path::new(&c.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.path.clone());
            SummaryChildResponse {
                path: c.path.clone(),
                name,
                kind: c.kind.clone(),
                summary: c.summary.clone(),
                summary_state: Some("done".into()),
            }
        })
        .collect();

    let crumb_responses: Vec<BreadcrumbResponse> = crumbs
        .iter()
        .map(|c| {
            let name = std::path::Path::new(&c.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.path.clone());
            BreadcrumbResponse {
                path: c.path.clone(),
                name,
                summary: c.summary.clone(),
            }
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
    let path = match params.path {
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"path required"})),
            )
                .into_response()
        }
        Some(p) => p,
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

async fn api_models_installed(State(state): State<AppState>) -> Response {
    let base = &state.config.describer.base_url;
    let url = format!("{base}/api/tags");
    let client = reqwest::Client::new();
    match client.get(&url).send().await {
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Ok(resp) => {
            let body: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response()
                }
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
    }
}

async fn api_models_pull(State(state): State<AppState>, Json(body): Json<PullRequest>) -> Response {
    let base = &state.config.describer.base_url;
    let url = format!("{base}/api/pull");
    let client = reqwest::Client::new();
    match client
        .post(&url)
        .json(&serde_json::json!({"name": body.name, "stream": true}))
        .send()
        .await
    {
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
        Ok(resp) => {
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
    }
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
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"Set INDEXA_WEB_ALLOW_KEY_EDIT=1 to enable API key editing via the web UI."})),
        )
            .into_response();
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
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"unknown provider"})),
            )
                .into_response()
        }
    }

    // Never log key material — log only the provider name.
    let provider = &body.provider;
    let _ = state.config.as_ref(); // keep state referenced
    match config::save(&cfg, &cfg_path) {
        Ok(()) => {
            tracing::info!("API key updated for provider={provider}");
            Json(serde_json::json!({"saved": true, "restart_required": true})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn api_roots(State(state): State<AppState>) -> Json<Vec<RootResponse>> {
    let store = state.store.lock().await;
    let paths = store.root_paths().unwrap_or_default();
    Json(
        paths
            .into_iter()
            .map(|p| {
                let name = std::path::Path::new(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone());
                RootResponse { path: p, name }
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
    Json(
        nodes
            .into_iter()
            .map(|n| TreeNodeResponse {
                path: n.path,
                name: n.name,
                kind: n.kind,
                child_count: n.child_count,
                byte_size: n.byte_size,
                summary_state: n.summary_state,
            })
            .collect(),
    )
}

async fn api_fs_ls(Query(params): Query<PathQuery>) -> Response {
    let raw = match params.path.as_deref() {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => {
            let home = directories::BaseDirs::new()
                .map(|b| b.home_dir().to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_owned());
            home
        }
    };

    // Security: reject path traversal and non-absolute paths.
    let canon = match std::fs::canonicalize(&raw) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "path not found" })),
            )
                .into_response();
        }
    };

    let home_canon = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .and_then(|h| std::fs::canonicalize(h).ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));

    // Clamp to HOME to prevent exposing system dirs.
    if !canon.starts_with(&home_canon) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "path outside home directory" })),
        )
            .into_response();
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

    match std::fs::read_dir(&canon) {
        Ok(rd) => {
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
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

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
    let path = match params.path.as_deref() {
        Some(p) => p.to_owned(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "path required" })),
            )
                .into_response();
        }
    };
    let mut store = state.store.lock().await;
    match store.mark_queue_state(&path, "pending", None) {
        Ok(_) => Json(serde_json::json!({ "queued": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
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
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "INDEXA_WEB_ALLOW_KEY_EDIT not set" })),
        )
            .into_response();
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn serve_ui() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_HTML,
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
        ..QaConfig::default()
    };

    // Step 1: embed query (async, no store lock needed).
    let query_vec = match state.embedder.embed(&body.question).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Step 2: sync store query (hold lock only for the synchronous call, no await).
    let hits = {
        let store = state.store.lock().await;
        match store.hybrid_search(
            &body.question,
            Some(&query_vec),
            &indexa_core::config::HybridMode::Rrf,
            None,
            qa_cfg.top_k,
            state.config.retrieval.rrf_k as f32,
        ) {
            Ok(h) => h,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
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

async fn api_job_scan(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let handle = Arc::new(JobHandle::new("scan", q.path.clone()));
    let id = handle.id;
    s.jobs.write().await.insert(id, handle.clone());
    let state = s.clone();
    tokio::spawn(async move { run_scan_phase(&state, &q.path, &handle).await });
    Json(JobStartResponse { job_id: id })
}

async fn api_job_deep(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let handle = Arc::new(JobHandle::new("deep", q.path.clone()));
    let id = handle.id;
    s.jobs.write().await.insert(id, handle.clone());
    let state = s.clone();
    let path = q.path.clone();
    tokio::spawn(async move {
        // Walk first then deep-index
        let path_buf = std::path::PathBuf::from(&path);
        let pb = path_buf.clone();
        let entries = tokio::task::spawn_blocking(move || walk(&pb, &WalkConfig::default()))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!(e)))
            .unwrap_or_default();
        run_deep_phase(&state, &path, &entries, &handle).await;
    });
    Json(JobStartResponse { job_id: id })
}

async fn api_job_summarize(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let handle = Arc::new(JobHandle::new("summarize", q.path.clone()));
    let id = handle.id;
    s.jobs.write().await.insert(id, handle.clone());
    let state = s.clone();
    tokio::spawn(async move { run_summarize_phase(&state, &q.path, q.passes, &handle).await });
    Json(JobStartResponse { job_id: id })
}

async fn api_job_index(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let handle = Arc::new(JobHandle::new("index", q.path.clone()));
    let id = handle.id;
    s.jobs.write().await.insert(id, handle.clone());
    let state = s.clone();
    tokio::spawn(async move { run_index_job(state, q.path, handle).await });
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
    let handle = {
        let jobs = s.jobs.read().await;
        match jobs.get(&id) {
            Some(h) => h.clone(),
            None => {
                return (StatusCode::NOT_FOUND, "job not found").into_response();
            }
        }
    };

    let history = handle.history.lock().unwrap().clone();
    let rx = handle.tx.subscribe();

    let replay = futures_util::stream::iter(history).map(|ev| {
        let data = serde_json::to_string(&ev).unwrap_or_default();
        Ok::<_, Infallible>(Event::default().data(data))
    });

    let live = BroadcastStream::new(rx)
        .filter_map(|r| async move { r.ok() })
        .map(|ev| {
            let data = serde_json::to_string(&ev).unwrap_or_default();
            Ok::<_, Infallible>(Event::default().data(data))
        });

    let stream = replay.chain(live);
    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

async fn api_job_delete(Path(id): Path<Uuid>, State(s): State<AppState>) -> impl IntoResponse {
    let mut jobs = s.jobs.write().await;
    if let Some(handle) = jobs.get(&id) {
        *handle.status.lock().unwrap() = JobStatus::Failed;
    }
    jobs.remove(&id);
    StatusCode::NO_CONTENT
}

// ── Job runner ────────────────────────────────────────────────────────────────

fn finalize_failed(handle: &Arc<JobHandle>, error: &str) {
    push(
        handle,
        JobEvent::Failed {
            error: error.to_owned(),
        },
    );
    *handle.status.lock().unwrap() = JobStatus::Failed;
}

async fn run_index_job(state: AppState, path: String, handle: Arc<JobHandle>) {
    // Phase 1: scan
    let path_buf = std::path::PathBuf::from(&path);
    let pb = path_buf.clone();
    let entries = match tokio::task::spawn_blocking(move || walk(&pb, &WalkConfig::default()))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!(e)))
    {
        Ok(e) => e,
        Err(e) => {
            finalize_failed(&handle, &e.to_string());
            return;
        }
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

/// Returns true on success.
async fn run_scan_phase(state: &AppState, path: &str, handle: &Arc<JobHandle>) -> bool {
    let pb = std::path::PathBuf::from(path);
    let entries = match tokio::task::spawn_blocking(move || walk(&pb, &WalkConfig::default()))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!(e)))
    {
        Ok(e) => e,
        Err(e) => {
            finalize_failed(handle, &e.to_string());
            return false;
        }
    };
    run_scan_phase_with_entries(state, path, &entries, handle).await
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
        finalize_failed(handle, &e.to_string());
        return false;
    }
    let _ = store.reconcile_entries(path, &live_paths);
    drop(store);

    push(
        handle,
        JobEvent::Progress {
            current: n,
            total: n,
            note: format!("{n} entries scanned"),
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

    push(
        handle,
        JobEvent::Start {
            kind: "deep".into(),
            path: path.to_owned(),
            total: Some(n_files),
        },
    );

    let embed_model = state.config.embedding.model.clone();
    let mut done = 0u64;

    for entry in &files {
        let path_str = entry.path.to_string_lossy().into_owned();

        let is_current = {
            let store = state.store.lock().await;
            store.chunks_are_current(&path_str).unwrap_or(false)
        };
        if is_current {
            done += 1;
            continue;
        }

        let ep = entry.path.clone();
        let extracted =
            match tokio::task::spawn_blocking(move || indexa_parsers::registry::parse(&ep)).await {
                Ok(Ok(e)) => e,
                _ => {
                    done += 1;
                    continue;
                }
            };

        if extracted.chunks.is_empty() {
            done += 1;
            continue;
        }

        let mut chunk_records = Vec::with_capacity(extracted.chunks.len());
        for chunk in &extracted.chunks {
            let embedding = state.embedder.embed(&chunk.text).await.ok();
            chunk_records.push(ChunkRecord {
                entry_path: path_str.clone(),
                seq: chunk.seq,
                heading: chunk.heading.clone(),
                text: chunk.text.clone(),
                language: chunk.language.clone(),
                embedding,
                embed_model: Some(embed_model.clone()),
            });
        }

        {
            let mut store = state.store.lock().await;
            let _ = store.upsert_chunks(&chunk_records);
        }

        done += 1;
        if done.is_multiple_of(10) || done == n_files {
            push(
                handle,
                JobEvent::Progress {
                    current: done,
                    total: n_files,
                    note: format!("{done}/{n_files} files embedded"),
                },
            );
        }
    }

    push(
        handle,
        JobEvent::Note {
            msg: format!("Deep index complete: {done} files"),
        },
    );
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
            finalize_failed(handle, &format!("failed to open index: {e}"));
            return;
        }
    };

    let enqueued = match enqueue_subtree(&mut job_store, &root) {
        Ok(n) => n,
        Err(e) => {
            finalize_failed(handle, &e.to_string());
            return;
        }
    };

    let mut done = 0usize;
    let mut errors = 0usize;
    loop {
        let item = match job_store.next_queue_item() {
            Ok(Some(i)) => i,
            Ok(None) => break,
            Err(e) => {
                finalize_failed(handle, &e.to_string());
                return;
            }
        };
        let r = process_queue_item_with_passes(
            &mut job_store,
            &describer,
            embedder.as_ref(),
            &item,
            &cfg,
            passes_override,
        )
        .await;
        match r {
            Ok(()) => done += 1,
            Err(_) => errors += 1,
        }
        if (done + errors).is_multiple_of(5) {
            push(
                handle,
                JobEvent::Progress {
                    current: (done + errors) as u64,
                    total: enqueued as u64,
                    note: format!("{done}/{enqueued} summaries"),
                },
            );
        }
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
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        embedder,
        llm,
        config: Arc::new(config),
        jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        db_path,
    };

    // Restrict CORS to localhost only — prevents drive-by sites from reading the
    // user's private index via cross-origin requests to the local server.
    let origin = format!("http://localhost:{port}")
        .parse::<axum::http::HeaderValue>()
        .expect("valid localhost origin header");

    let app = Router::new()
        .route("/", get(serve_ui))
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
        .route("/api/jobs/:id", delete(api_job_delete))
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

// ── Embedded UI ───────────────────────────────────────────────────────────────

const UI_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Indexa — Local Context Engine</title>
<style>
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
  :root {
    --bg: #0d1117; --surface: #161b22; --border: #30363d;
    --text: #e6edf3; --muted: #8b949e; --accent: #58a6ff;
    --green: #3fb950; --red: #f85149; --orange: #d29922;
    font-size: 14px;
  }
  body { background: var(--bg); color: var(--text); font-family: 'SF Mono', 'Cascadia Code', 'Fira Code', monospace; min-height: 100vh; display: flex; flex-direction: column; }
  header { background: var(--surface); border-bottom: 1px solid var(--border); padding: 10px 20px; display: flex; align-items: center; gap: 12px; flex-shrink: 0; }
  header h1 { font-size: 17px; font-weight: 600; color: var(--accent); letter-spacing: -0.5px; }
  header .stats { color: var(--muted); font-size: 11px; }
  header .tabs { margin-left: auto; display: flex; gap: 2px; }
  header .tab { background: none; border: 1px solid transparent; border-radius: 6px; color: var(--muted); padding: 4px 12px; font-family: inherit; font-size: 12px; cursor: pointer; }
  header .tab.active { border-color: var(--border); color: var(--text); background: var(--bg); }
  .layout { display: grid; grid-template-columns: 260px 1fr; flex: 1; overflow: hidden; }

  /* ── Tree sidebar ── */
  .tree-pane { background: var(--surface); border-right: 1px solid var(--border); overflow-y: auto; display: flex; flex-direction: column; }
  .tree-header { padding: 10px 14px; font-size: 11px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted); border-bottom: 1px solid var(--border); display: flex; justify-content: space-between; align-items: center; flex-shrink: 0; }
  .tree-header .queue-badge { background: var(--border); border-radius: 10px; padding: 1px 7px; font-size: 10px; color: var(--text); }
  .tree-list { flex: 1; overflow-y: auto; }
  .tree-node { padding: 5px 0; cursor: pointer; user-select: none; }
  .tree-node-row { display: flex; align-items: center; gap: 6px; padding: 3px 12px; border-radius: 4px; }
  .tree-node-row:hover { background: rgba(88,166,255,0.06); }
  .tree-node-row.selected { background: rgba(88,166,255,0.12); }
  .tree-toggle { width: 14px; text-align: center; color: var(--muted); font-size: 10px; flex-shrink: 0; }
  .tree-icon { flex-shrink: 0; }
  .tree-label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; }
  .tree-badge { font-size: 10px; flex-shrink: 0; }
  .tree-badge.done { color: var(--green); }
  .tree-badge.pending { color: var(--orange); animation: pulse 2s ease-in-out infinite; }
  .tree-badge.failed { color: var(--red); }
  .tree-children { padding-left: 16px; }
  @keyframes pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.35; } }
  .tree-search { padding: 6px 8px; border-bottom: 1px solid var(--border); flex-shrink: 0; display: flex; gap: 4px; }
  .tree-search input { flex: 1; background: var(--bg); border: 1px solid var(--border); border-radius: 5px; color: var(--text); font-family: inherit; font-size: 12px; padding: 4px 8px; outline: none; }
  .tree-search input:focus { border-color: var(--accent); }
  .tree-search button { background: none; border: none; color: var(--muted); cursor: pointer; font-size: 12px; padding: 2px 4px; }
  .add-root-btn { background: none; border: 1px solid var(--border); border-radius: 4px; color: var(--accent); font-size: 14px; font-weight: 600; cursor: pointer; padding: 0 5px; line-height: 16px; }
  .add-root-btn:hover { background: rgba(88,166,255,0.1); }
  .empty-state { padding: 24px 16px; text-align: center; color: var(--muted); font-size: 12px; line-height: 1.8; }
  .empty-state .cta-link { color: var(--accent); cursor: pointer; }
  .modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.55); z-index: 100; display: none; align-items: center; justify-content: center; }
  .modal-overlay.open { display: flex; }
  .modal { background: var(--surface); border: 1px solid var(--border); border-radius: 10px; padding: 22px; width: 460px; max-width: 96vw; }
  .modal h2 { font-size: 14px; font-weight: 600; margin-bottom: 14px; }
  .modal .path-row { display: flex; gap: 6px; margin-bottom: 8px; }
  .modal .path-row input { flex: 1; background: var(--bg); border: 1px solid var(--border); border-radius: 5px; color: var(--text); font-family: inherit; font-size: 12px; padding: 5px 9px; outline: none; }
  .modal .path-row input:focus { border-color: var(--accent); }
  .modal .path-row button { background: none; border: 1px solid var(--border); border-radius: 5px; color: var(--muted); font-family: inherit; font-size: 11px; cursor: pointer; padding: 4px 8px; }
  .fs-browser { background: var(--bg); border: 1px solid var(--border); border-radius: 5px; max-height: 170px; overflow-y: auto; margin-bottom: 12px; }
  .fs-entry { padding: 5px 10px; cursor: pointer; font-size: 12px; color: var(--text); }
  .fs-entry:hover { background: rgba(88,166,255,0.08); }
  .modal-actions { display: flex; gap: 8px; justify-content: flex-end; }
  .modal-btn { border: 1px solid var(--border); border-radius: 5px; padding: 5px 14px; font-family: inherit; font-size: 12px; cursor: pointer; background: none; color: var(--text); }
  .modal-btn.primary { background: var(--accent); border-color: var(--accent); color: #0d1117; font-weight: 600; }
  .modal-btn:hover:not(.primary) { border-color: var(--accent); color: var(--accent); }

  /* ── Right panel (summary or chat) ── */
  .right-panel { display: flex; flex-direction: column; overflow: hidden; }

  /* Summary view */
  .summary-view { flex: 1; overflow-y: auto; padding: 24px; display: none; }
  .summary-view.visible { display: block; }
  .crumbs { font-size: 11px; color: var(--muted); margin-bottom: 16px; }
  .crumbs .crumb { cursor: pointer; color: var(--accent); text-decoration: none; }
  .crumbs .crumb:hover { text-decoration: underline; }
  .crumbs .sep { margin: 0 6px; color: var(--border); }
  .summary-header { display: flex; align-items: flex-start; gap: 10px; margin-bottom: 12px; }
  .summary-title { font-size: 16px; font-weight: 600; color: var(--text); }
  .summary-meta { font-size: 11px; color: var(--muted); margin-bottom: 16px; }
  .summary-text { color: var(--text); line-height: 1.7; font-size: 13px; margin-bottom: 24px; background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 14px; }
  .summary-pending { color: var(--orange); font-style: italic; padding: 14px; background: var(--surface); border: 1px solid var(--border); border-radius: 8px; }
  .children-section h3 { font-size: 11px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted); margin-bottom: 10px; }
  .child-item { background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: 10px 12px; margin-bottom: 6px; cursor: pointer; }
  .child-item:hover { border-color: var(--accent); }
  .child-row { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; }
  .child-name { font-size: 12px; font-weight: 500; color: var(--accent); }
  .child-summary { font-size: 11px; color: var(--muted); line-height: 1.5; }
  .enqueue-btn { background: none; border: 1px solid var(--border); border-radius: 6px; color: var(--muted); padding: 4px 10px; font-family: inherit; font-size: 11px; cursor: pointer; margin-top: 8px; }
  .enqueue-btn:hover { border-color: var(--accent); color: var(--accent); }

  /* Chat view */
  .chat-view { flex: 1; display: flex; flex-direction: column; overflow: hidden; }
  .chat-area { flex: 1; overflow-y: auto; padding: 24px; display: flex; flex-direction: column; gap: 20px; }
  .welcome { max-width: 600px; margin: auto; text-align: center; padding: 60px 0; }
  .welcome h2 { font-size: 22px; color: var(--text); margin-bottom: 12px; font-weight: 500; }
  .welcome p { color: var(--muted); line-height: 1.6; }
  .welcome code { color: var(--accent); }
  .msg { max-width: 800px; width: 100%; }
  .msg.user .bubble { background: var(--accent); color: #0d1117; border-radius: 12px 12px 2px 12px; padding: 10px 14px; display: inline-block; }
  .msg.assistant .bubble { background: var(--surface); border: 1px solid var(--border); border-radius: 2px 12px 12px 12px; padding: 14px; white-space: pre-wrap; line-height: 1.7; }
  .msg.user { align-self: flex-end; }
  .msg.assistant { align-self: flex-start; }
  .sources { margin-top: 12px; }
  .sources h4 { font-size: 11px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted); margin-bottom: 8px; }
  .source-item { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; margin-bottom: 6px; font-size: 12px; }
  .source-item .path { color: var(--accent); font-weight: 500; }
  .source-item .heading { color: var(--orange); margin-left: 8px; }
  .source-item .snippet { color: var(--muted); margin-top: 4px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .thinking { color: var(--muted); font-style: italic; animation: pulse 1.5s ease-in-out infinite; }
  .input-bar { background: var(--surface); border-top: 1px solid var(--border); padding: 14px 20px; display: flex; gap: 10px; flex-shrink: 0; }
  .input-bar input { flex: 1; background: var(--bg); border: 1px solid var(--border); border-radius: 8px; color: var(--text); padding: 9px 12px; font-family: inherit; font-size: 14px; outline: none; }
  .input-bar input:focus { border-color: var(--accent); }
  .input-bar input::placeholder { color: var(--muted); }
  .input-bar button { background: var(--accent); color: #0d1117; border: none; border-radius: 8px; padding: 9px 18px; font-family: inherit; font-size: 14px; font-weight: 600; cursor: pointer; }
  .input-bar button:hover { opacity: 0.85; }
  .input-bar button:disabled { opacity: 0.4; cursor: default; }
  ::-webkit-scrollbar { width: 6px; } ::-webkit-scrollbar-track { background: transparent; } ::-webkit-scrollbar-thumb { background: var(--border); border-radius: 3px; }

  /* ── Jobs panel ── */
  .jobs-panel { border-top: 1px solid var(--border); flex-shrink: 0; font-size: 11px; }
  .jobs-panel-header { padding: 5px 14px; color: var(--muted); text-transform: uppercase; letter-spacing: 1px; font-size: 10px; }
  .job-row { padding: 4px 14px; display: flex; align-items: center; gap: 6px; border-bottom: 1px solid var(--border); }
  .job-row:last-child { border-bottom: none; }
  .job-kind { color: var(--accent); width: 68px; flex-shrink: 0; }
  .job-label { flex: 1; color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .job-note { color: var(--muted); font-size: 10px; white-space: nowrap; flex-shrink: 0; }
  .job-note.done { color: var(--green); }
  .job-note.failed { color: var(--red); }
  .job-note.running { animation: pulse 1.5s ease-in-out infinite; }

  /* ── Settings view ── */
  .settings-view { flex: 1; overflow-y: auto; padding: 24px; display: none; }
  .settings-view.visible { display: block; }
  #map-table th { text-align: left; color: var(--muted); font-weight: 500; font-size: 11px; text-transform: uppercase; letter-spacing: .5px; padding: 4px 8px; border-bottom: 1px solid var(--border); }
  #map-table td { padding: 5px 8px; border-bottom: 1px solid var(--border); color: var(--text); }
  #map-table tr:last-child td { border-bottom: none; }
  .settings-section { margin-bottom: 28px; }
  .settings-section h2 { font-size: 13px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted); margin-bottom: 14px; border-bottom: 1px solid var(--border); padding-bottom: 8px; }
  .model-list { display: flex; flex-direction: column; gap: 6px; }
  .model-row { background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; display: flex; align-items: center; gap: 10px; font-size: 12px; }
  .model-name { flex: 1; color: var(--text); }
  .model-size { color: var(--muted); font-size: 11px; }
  .pull-row { display: flex; gap: 8px; margin-top: 10px; }
  .pull-row input { flex: 1; background: var(--bg); border: 1px solid var(--border); border-radius: 6px; color: var(--text); padding: 7px 10px; font-family: inherit; font-size: 12px; outline: none; }
  .pull-row input:focus { border-color: var(--accent); }
  .pull-row button { background: var(--accent); color: #0d1117; border: none; border-radius: 6px; padding: 7px 14px; font-family: inherit; font-size: 12px; font-weight: 600; cursor: pointer; white-space: nowrap; }
  .pull-row button:disabled { opacity: 0.4; cursor: default; }
  .pull-progress { margin-top: 8px; font-size: 11px; color: var(--muted); white-space: pre-wrap; background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 8px 10px; max-height: 120px; overflow-y: auto; display: none; }
  .key-rows { display: flex; flex-direction: column; gap: 10px; }
  .key-row { display: flex; align-items: center; gap: 10px; }
  .key-label { width: 100px; font-size: 12px; color: var(--muted); flex-shrink: 0; }
  .key-input { flex: 1; background: var(--bg); border: 1px solid var(--border); border-radius: 6px; color: var(--text); padding: 7px 10px; font-family: inherit; font-size: 12px; outline: none; }
  .key-input:focus { border-color: var(--accent); }
  .key-input:disabled { opacity: 0.4; }
  .key-row .btn-sm { background: none; border: 1px solid var(--border); border-radius: 6px; color: var(--muted); padding: 5px 10px; font-family: inherit; font-size: 11px; cursor: pointer; white-space: nowrap; }
  .key-row .btn-sm:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
  .key-row .btn-sm:disabled { opacity: 0.4; cursor: default; }
  .key-set-badge { font-size: 10px; color: var(--green); flex-shrink: 0; }
  .key-gate-notice { font-size: 11px; color: var(--orange); background: var(--surface); border: 1px solid var(--border); border-radius: 6px; padding: 10px 12px; margin-bottom: 12px; }
  .settings-note { font-size: 11px; color: var(--muted); margin-top: 8px; line-height: 1.5; }
</style>
</head>
<body>
<header>
  <h1>&#x2B21; Indexa</h1>
  <span class="stats" id="stats">Loading&#x2026;</span>
  <div class="tabs">
    <button class="tab" id="tab-tree" onclick="switchTab('tree')">Tree</button>
    <button class="tab active" id="tab-chat" onclick="switchTab('chat')">Ask</button>
    <button class="tab" id="tab-map" onclick="switchTab('map')">Map</button>
    <button class="tab" id="tab-settings" onclick="switchTab('settings')">Settings</button>
  </div>
</header>
<div class="layout">
  <!-- Tree sidebar (always visible) -->
  <div class="tree-pane" id="tree-pane">
    <div class="tree-header">
      <span>Folder tree</span>
      <span class="queue-badge" id="queue-badge" style="display:none"></span>
      <button class="add-root-btn" onclick="openAddRoot()" title="Add root folder">+</button>
    </div>
    <div class="tree-search">
      <input type="text" id="search-input" placeholder="Search files&#x2026;" autocomplete="off" oninput="onSearchInput(this.value)">
      <button id="search-clear" onclick="clearSearchInput()" style="display:none" title="Clear">&#x2715;</button>
    </div>
    <div class="tree-list" id="tree-list"></div>
    <div class="jobs-panel" id="jobs-panel" style="display:none">
      <div class="jobs-panel-header">Jobs</div>
      <div id="jobs-list"></div>
    </div>
  </div>
  <!-- Right panel switches between summary view, chat view, and settings view -->
  <div class="right-panel">
    <div class="summary-view" id="summary-view"></div>
    <div class="settings-view" id="settings-view">
      <div class="settings-section">
        <h2>Local Models (Ollama)</h2>
        <div id="models-list" class="model-list"><div style="color:var(--muted);font-size:12px">Loading…</div></div>
        <div class="pull-row">
          <input type="text" id="pull-input" placeholder="Model name (e.g. gemma3:4b, nomic-embed-text)" autocomplete="off" list="model-suggestions">
          <datalist id="model-suggestions">
            <option value="gemma3:4b">
            <option value="gemma3:12b">
            <option value="gemma3:27b">
            <option value="nomic-embed-text">
            <option value="qwen2.5-coder:7b">
            <option value="mistral:7b">
          </datalist>
          <button id="pull-btn" onclick="pullModel()">Pull</button>
        </div>
        <div id="pull-progress" class="pull-progress"></div>
        <p class="settings-note">Models are stored by Ollama. After pulling, update <code>[describer] model</code> in <code>~/.indexa/config.toml</code> to use the new model.</p>
      </div>
      <div class="settings-section">
        <h2>Cloud Provider API Keys</h2>
        <div id="key-gate-notice" class="key-gate-notice" style="display:none">
          API key editing is disabled. To enable: <code>INDEXA_WEB_ALLOW_KEY_EDIT=1 indexa serve</code>
        </div>
        <div class="key-rows">
          <div class="key-row">
            <span class="key-label">OpenAI</span>
            <input type="password" class="key-input" id="key-openai" placeholder="sk-…" autocomplete="off">
            <span class="key-set-badge" id="badge-openai"></span>
            <button class="btn-sm" onclick="saveKey('openai')">Save</button>
            <button class="btn-sm" onclick="clearKey('openai')">Clear</button>
          </div>
          <div class="key-row">
            <span class="key-label">Anthropic</span>
            <input type="password" class="key-input" id="key-anthropic" placeholder="sk-ant-…" autocomplete="off">
            <span class="key-set-badge" id="badge-anthropic"></span>
            <button class="btn-sm" onclick="saveKey('anthropic')">Save</button>
            <button class="btn-sm" onclick="clearKey('anthropic')">Clear</button>
          </div>
          <div class="key-row">
            <span class="key-label">Google</span>
            <input type="password" class="key-input" id="key-google" placeholder="AIza…" autocomplete="off">
            <span class="key-set-badge" id="badge-google"></span>
            <button class="btn-sm" onclick="saveKey('google')">Save</button>
            <button class="btn-sm" onclick="clearKey('google')">Clear</button>
          </div>
        </div>
        <p class="settings-note">Keys are saved to <code>~/.indexa/config.toml</code> (0600 permissions). Restart <code>indexa serve</code> after saving to apply.</p>
      </div>
      <div class="settings-section">
        <h2>Refinement Passes</h2>
        <div class="key-rows">
          <div class="key-row">
            <span class="key-label">First-time build</span>
            <input type="number" class="key-input" id="passes-first" min="1" max="3" value="2" style="width:60px">
            <span class="key-set-badge" style="color:var(--muted);font-size:11px">default 2</span>
          </div>
          <div class="key-row">
            <span class="key-label">Refresh run</span>
            <input type="number" class="key-input" id="passes-refresh" min="1" max="3" value="1" style="width:60px">
            <span class="key-set-badge" style="color:var(--muted);font-size:11px">default 1</span>
          </div>
        </div>
        <div style="display:flex;gap:8px;align-items:center;margin-top:8px">
          <button class="btn-sm" onclick="savePasses()">Save passes</button>
          <span id="passes-status" style="font-size:11px;color:var(--muted)"></span>
        </div>
        <p class="settings-note">More passes = higher context quality at the cost of LLM calls. Set in <code>[describer] passes-first</code> / <code>passes-refresh</code> in config.toml. Cap is 3 (Self-Refine research: quality degrades above 3 passes).</p>
      </div>
    </div>
    <div class="chat-view" id="chat-view">
      <div class="chat-area" id="chat">
        <div class="welcome">
          <h2>Your local context, on tap</h2>
          <p>Indexa has built context for your files. Ask, export to your AI tool, or browse the tree.<br><br>
          <code>&ldquo;where are my tax documents?&rdquo;</code> &nbsp;&middot;&nbsp; <code>&ldquo;where is auth handled in this repo?&rdquo;</code><br><br>
          Click any folder in the tree to explore its context summary.</p>
        </div>
      </div>
      <div class="input-bar">
        <input type="text" id="q" placeholder="Ask your local context… (⌘K)" autocomplete="off">
        <button id="send">Ask</button>
      </div>
    </div>
    <div class="map-view" id="map-view" style="display:none;flex:1;overflow-y:auto;padding:24px">
      <h2 style="font-size:14px;font-weight:600;margin-bottom:16px;color:var(--text)">Index map</h2>
      <table id="map-table" style="width:100%;border-collapse:collapse;font-size:12px;align-self:flex-start"></table>
    </div>
  </div>
</div>
<!-- Add-Root modal -->
<div class="modal-overlay" id="add-root-modal" onclick="if(event.target===this)closeAddRoot()">
  <div class="modal">
    <h2>Add Root Folder</h2>
    <div class="path-row">
      <input type="text" id="add-root-path" placeholder="/Users/you/Documents" autocomplete="off"
        oninput="onRootPathInput(this.value)">
      <button onclick="browseFsTo(document.getElementById('add-root-path').value)">Browse</button>
    </div>
    <div class="fs-browser" id="fs-browser"><div class="fs-entry" style="color:var(--muted)">Loading&#x2026;</div></div>
    <div class="modal-actions">
      <button class="modal-btn" onclick="closeAddRoot()">Cancel</button>
      <button class="modal-btn primary" onclick="startIndexRoot()">Index this folder</button>
    </div>
  </div>
</div>
<script>
/* ── State ── */
let currentTab = 'chat';
let selectedPath = null;
const expandedPaths = new Set();

/* ── Tab switching ── */
function switchTab(tab) {
  currentTab = tab;
  document.getElementById('tab-tree').classList.toggle('active', tab === 'tree');
  document.getElementById('tab-chat').classList.toggle('active', tab === 'chat');
  document.getElementById('tab-map').classList.toggle('active', tab === 'map');
  document.getElementById('tab-settings').classList.toggle('active', tab === 'settings');
  document.getElementById('summary-view').classList.toggle('visible', tab === 'tree' && selectedPath !== null);
  document.getElementById('chat-view').style.display = tab === 'chat' ? 'flex' : 'none';
  document.getElementById('map-view').style.display = tab === 'map' ? 'flex' : 'none';
  document.getElementById('settings-view').classList.toggle('visible', tab === 'settings');
  if (tab === 'settings') loadSettings();
  if (tab === 'map') loadMap();
}

/* ── Stats ── */
async function loadStats() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();
    document.getElementById('stats').textContent =
      d.entries.toLocaleString() + ' files · ' + d.chunks.toLocaleString() + ' chunks';
  } catch(e) { document.getElementById('stats').textContent = 'No index yet'; }
}

/* ── Tree ── */
async function loadTreeLevel(parentPath, container) {
  container.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:11px">Loading…</div>';
  try {
    const url = '/api/tree?path=' + encodeURIComponent(parentPath);
    const r = await fetch(url);
    const nodes = await r.json();
    if (!nodes.length) {
      container.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:11px">Empty</div>';
      return;
    }
    container.innerHTML = '';
    nodes.forEach(function(node) { container.appendChild(buildTreeNode(node)); });
  } catch(e) {
    container.innerHTML = '<div style="padding:6px 12px;color:var(--red);font-size:11px">Error loading</div>';
  }
}

function badgeFor(state) {
  if (!state) return '';
  if (state === 'done') return '<span class="tree-badge done" title="Summarized">✓</span>';
  if (state === 'failed') return '<span class="tree-badge failed" title="Summary failed">✗</span>';
  return '<span class="tree-badge pending" title="Summary pending">⏳</span>';
}

function buildTreeNode(node) {
  const wrap = document.createElement('div');
  wrap.className = 'tree-node';
  wrap.dataset.path = node.path;

  const isDir = node.kind === 'dir';
  const icon = isDir ? '📁' : '📄';
  const badge = badgeFor(node.summary_state);
  const toggle = isDir ? '<span class="tree-toggle">▸</span>' : '<span class="tree-toggle"></span>';

  const row = document.createElement('div');
  row.className = 'tree-node-row' + (node.path === selectedPath ? ' selected' : '');
  row.innerHTML = toggle + '<span class="tree-icon">' + icon + '</span>' +
    '<span class="tree-label" title="' + escapeHtml(node.name) + '">' + escapeHtml(node.name) + '</span>' +
    badge;

  const childContainer = document.createElement('div');
  childContainer.className = 'tree-children';
  childContainer.style.display = 'none';

  row.addEventListener('click', function(e) {
    e.stopPropagation();
    // Select
    document.querySelectorAll('.tree-node-row.selected').forEach(function(el) { el.classList.remove('selected'); });
    row.classList.add('selected');
    selectedPath = node.path;
    showSummary(node.path);

    // Toggle expand for dirs
    if (isDir) {
      const isExpanded = expandedPaths.has(node.path);
      if (isExpanded) {
        expandedPaths.delete(node.path);
        childContainer.style.display = 'none';
        row.querySelector('.tree-toggle').textContent = '▸';
      } else {
        expandedPaths.add(node.path);
        childContainer.style.display = 'block';
        row.querySelector('.tree-toggle').textContent = '▾';
        if (!childContainer.dataset.loaded) {
          childContainer.dataset.loaded = '1';
          loadTreeLevel(node.path, childContainer);
        }
      }
    }
  });

  wrap.appendChild(row);
  if (isDir) wrap.appendChild(childContainer);
  return wrap;
}

async function initTree() {
  const list = document.getElementById('tree-list');
  list.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:11px">Loading&#x2026;</div>';
  try {
    const r = await fetch('/api/roots');
    const roots = await r.json();
    if (!roots.length) {
      list.innerHTML = '<div class="empty-state">No indexed roots yet.<br><span class="cta-link" onclick="openAddRoot()">+ Add Root</span> to get started, or run <code>indexa scan &lt;path&gt;</code> in your terminal.</div>';
      return;
    }
    list.innerHTML = '';
    roots.forEach(function(root) {
      list.appendChild(buildTreeNode({path: root.path, name: root.name, kind: 'dir', summary_state: null}));
    });
  } catch(e) {
    list.innerHTML = '<div style="padding:6px 12px;color:var(--red);font-size:11px">Error loading tree</div>';
  }
}

/* ── Search ── */
var _searchTimer = null;
function onSearchInput(val) {
  document.getElementById('search-clear').style.display = val ? '' : 'none';
  clearTimeout(_searchTimer);
  if (!val.trim()) { initTree(); return; }
  _searchTimer = setTimeout(function() { doSearch(val.trim()); }, 200);
}
function clearSearchInput() {
  document.getElementById('search-input').value = '';
  document.getElementById('search-clear').style.display = 'none';
  initTree();
}
async function doSearch(q) {
  const list = document.getElementById('tree-list');
  list.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:11px">Searching&#x2026;</div>';
  try {
    const r = await fetch('/api/search?q=' + encodeURIComponent(q) + '&limit=50');
    const nodes = await r.json();
    if (!nodes.length) {
      list.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:11px">No results</div>';
      return;
    }
    list.innerHTML = '';
    nodes.forEach(function(node) { list.appendChild(buildTreeNode(node)); });
  } catch(e) {
    list.innerHTML = '<div style="padding:6px 12px;color:var(--red);font-size:11px">Search error</div>';
  }
}

/* ── Add-Root modal ── */
var _rootPathDebounce = null;
function openAddRoot() {
  document.getElementById('add-root-modal').classList.add('open');
  browseFsTo('');
}
function closeAddRoot() {
  document.getElementById('add-root-modal').classList.remove('open');
}
function onRootPathInput(val) {
  clearTimeout(_rootPathDebounce);
  _rootPathDebounce = setTimeout(function() { browseFsTo(val); }, 350);
}
async function browseFsTo(path) {
  if (path) document.getElementById('add-root-path').value = path;
  const browser = document.getElementById('fs-browser');
  browser.innerHTML = '<div class="fs-entry" style="color:var(--muted)">Loading&#x2026;</div>';
  try {
    const r = await fetch('/api/fs/ls?path=' + encodeURIComponent(path || ''));
    if (!r.ok) {
      const d = await r.json().catch(function(){return {};});
      browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">' + escapeHtml(d.error || 'Permission denied') + '</div>';
      return;
    }
    const entries = await r.json();
    browser.innerHTML = '';
    if (path) {
      const up = document.createElement('div');
      up.className = 'fs-entry';
      up.style.color = 'var(--muted)';
      up.innerHTML = '⤴ ..';
      up.onclick = function() {
        const parts = path.replace(/\/$/, '').split('/');
        parts.pop();
        browseFsTo(parts.join('/') || '/');
      };
      browser.appendChild(up);
    }
    if (!entries.length) {
      const empty = document.createElement('div');
      empty.className = 'fs-entry';
      empty.style.color = 'var(--muted)';
      empty.textContent = 'No subdirectories';
      browser.appendChild(empty);
    } else {
      entries.forEach(function(e) {
        const el = document.createElement('div');
        el.className = 'fs-entry';
        el.innerHTML = '📁 ' + escapeHtml(e.name);
        el.onclick = function() { browseFsTo(e.path); };
        browser.appendChild(el);
      });
    }
  } catch(err) {
    browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">Error</div>';
  }
}
async function startIndexRoot() {
  const path = document.getElementById('add-root-path').value.trim();
  if (!path) { alert('Enter a path first.'); return; }
  try {
    const r = await fetch('/api/jobs/index?path=' + encodeURIComponent(path), { method: 'POST' });
    const d = await r.json();
    closeAddRoot();
    subscribeJob(d.job_id, path);
  } catch(e) {
    alert('Failed to start indexing: ' + e.message);
  }
}

/* ── Jobs panel ── */
var activeJobs = {}; // job_id → { es, row }

function getOrCreateJobRow(jobId) {
  if (activeJobs[jobId]) return activeJobs[jobId].row;
  const panel = document.getElementById('jobs-panel');
  panel.style.display = '';
  const list = document.getElementById('jobs-list');
  const row = document.createElement('div');
  row.className = 'job-row';
  row.id = 'job-row-' + jobId;
  row.innerHTML = '<span class="job-kind">…</span>' +
    '<span class="job-label">Starting…</span>' +
    '<span class="job-note running">●</span>';
  list.appendChild(row);
  activeJobs[jobId] = { row: row, es: null };
  return row;
}

function subscribeJob(jobId, path) {
  const row = getOrCreateJobRow(jobId);
  row.querySelector('.job-label').textContent = (path || '').split('/').pop() || path || jobId;

  const es = new EventSource('/api/jobs/' + jobId + '/events');
  activeJobs[jobId].es = es;

  es.onmessage = function(e) {
    try {
      const ev = JSON.parse(e.data);
      const kindEl = row.querySelector('.job-kind');
      const noteEl = row.querySelector('.job-note');
      if (ev.type === 'start') {
        kindEl.textContent = ev.kind;
        noteEl.className = 'job-note running';
        noteEl.textContent = ev.total ? '0/' + ev.total : '…';
      } else if (ev.type === 'progress') {
        noteEl.textContent = ev.current + '/' + ev.total;
      } else if (ev.type === 'note') {
        noteEl.textContent = ev.msg.slice(0, 30);
      } else if (ev.type === 'done') {
        noteEl.className = 'job-note done';
        noteEl.textContent = '✓ ' + ev.summary;
        es.close();
        setTimeout(function() {
          row.remove();
          delete activeJobs[jobId];
          if (!document.getElementById('jobs-list').children.length) {
            document.getElementById('jobs-panel').style.display = 'none';
          }
          initTree(); // refresh tree to show new root
          loadStats();
        }, 5000);
      } else if (ev.type === 'failed') {
        noteEl.className = 'job-note failed';
        noteEl.textContent = '✗ ' + ev.error.slice(0, 40);
        es.close();
      }
    } catch(_) {}
  };
  es.onerror = function() {
    const noteEl = row.querySelector('.job-note');
    if (noteEl.className.indexOf('done') === -1 && noteEl.className.indexOf('failed') === -1) {
      noteEl.className = 'job-note failed';
      noteEl.textContent = 'connection lost';
    }
    es.close();
  };
}

async function reconnectInFlightJobs() {
  try {
    const r = await fetch('/api/jobs');
    const jobs = await r.json();
    jobs.forEach(function(j) {
      if (j.status === 'running') {
        subscribeJob(j.job_id, j.path);
      }
    });
  } catch(_) {}
}

/* ── Summary view ── */
async function showSummary(path) {
  switchTab('tree');
  const view = document.getElementById('summary-view');
  view.innerHTML = '<div class="summary-pending">Loading summary…</div>';
  view.classList.add('visible');

  try {
    const r = await fetch('/api/summary?path=' + encodeURIComponent(path));
    const d = await r.json();

    if (d.error === 'no summary' || d.pending) {
      view.innerHTML = renderNoPendingSummary(path);
      return;
    }
    if (d.error) {
      view.innerHTML = '<div class="summary-pending" style="color:var(--red)">' + escapeHtml(d.error) + '</div>';
      return;
    }

    view.innerHTML = renderSummary(d);

    // Wire child clicks
    view.querySelectorAll('.child-item[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    // Wire breadcrumb clicks
    view.querySelectorAll('.crumb[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    // Wire enqueue button
    const enqBtn = view.querySelector('#enqueue-btn');
    if (enqBtn) {
      enqBtn.addEventListener('click', async function() {
        enqBtn.disabled = true;
        enqBtn.textContent = 'Queued…';
        await fetch('/api/summarize?path=' + encodeURIComponent(path), { method: 'POST' });
        setTimeout(function() { showSummary(path); }, 2000);
      });
    }
  } catch(e) {
    view.innerHTML = '<div class="summary-pending" style="color:var(--red)">Error: ' + escapeHtml(e.message) + '</div>';
  }
}

function renderNoPendingSummary(path) {
  const name = path.split('/').pop() || path;
  return '<div class="summary-text">' +
    '<div style="color:var(--muted);margin-bottom:12px">No summary yet for <strong>' + escapeHtml(name) + '</strong></div>' +
    '<button class="enqueue-btn" id="enqueue-btn">Generate summary</button>' +
    '</div>';
}

function renderSummary(d) {
  const name = d.path.split('/').pop() || d.path;
  const icon = d.kind === 'dir' ? '📁' : '📄';

  let crumbHtml = '';
  if (d.crumbs && d.crumbs.length) {
    crumbHtml = '<div class="crumbs">' +
      d.crumbs.map(function(c) {
        return '<a class="crumb" data-path="' + escapeAttr(c.path) + '">' + escapeHtml(c.name) + '</a>';
      }).join('<span class="sep">›</span>') +
      '<span class="sep">›</span><span>' + escapeHtml(name) + '</span></div>';
  }

  let childrenHtml = '';
  if (d.children && d.children.length) {
    childrenHtml = '<div class="children-section"><h3>Contents (' + d.children.length + ')</h3>' +
      d.children.map(function(c) {
        const cIcon = c.kind === 'dir' ? '📁' : '📄';
        return '<div class="child-item" data-path="' + escapeAttr(c.path) + '">' +
          '<div class="child-row"><span>' + cIcon + '</span><span class="child-name">' + escapeHtml(c.name) + '</span></div>' +
          '<div class="child-summary">' + escapeHtml(c.summary) + '</div>' +
          '</div>';
      }).join('') + '</div>';
  }

  const ts = d.generated_at ? new Date(d.generated_at * 1000).toLocaleDateString() : '';
  return crumbHtml +
    '<div class="summary-header"><span style="font-size:20px">' + icon + '</span>' +
    '<span class="summary-title">' + escapeHtml(name) + '</span></div>' +
    '<div class="summary-meta">Model: ' + escapeHtml(d.model) + (ts ? ' · ' + ts : '') + '</div>' +
    '<div class="summary-text">' + escapeHtml(d.summary) + '</div>' +
    childrenHtml;
}

/* ── Chat / Ask ── */
const chat = document.getElementById('chat');
const qInput = document.getElementById('q');
const sendBtn = document.getElementById('send');

function appendMsg(role, html) {
  const welcome = chat.querySelector('.welcome');
  if (welcome) welcome.remove();
  const div = document.createElement('div');
  div.className = 'msg ' + role;
  div.innerHTML = '<div class="bubble">' + html + '</div>';
  chat.appendChild(div);
  chat.scrollTop = chat.scrollHeight;
  return div;
}

async function doAsk() {
  const q = qInput.value.trim();
  if (!q) return;
  qInput.value = '';
  sendBtn.disabled = true;
  switchTab('chat');

  appendMsg('user', escapeHtml(q));
  const thinking = appendMsg('assistant', '<span class="thinking">Thinking…</span>');

  try {
    const r = await fetch('/api/ask', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ question: q })
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || 'Request failed');

    let html = escapeHtml(d.answer);
    if (d.sources && d.sources.length > 0) {
      html += '<div class="sources"><h4>Sources</h4>' +
        d.sources.map(function(s) {
          return '<div class="source-item"><span class="path">' + escapeHtml(s.path) + '</span>' +
            (s.heading ? '<span class="heading">' + escapeHtml(s.heading) + '</span>' : '') +
            '<div class="snippet">' + escapeHtml(s.snippet) + '</div></div>';
        }).join('') + '</div>';
    }
    thinking.querySelector('.bubble').innerHTML = html;
  } catch(err) {
    thinking.querySelector('.bubble').innerHTML = '<span style="color:var(--red)">' + escapeHtml(err.message) + '</span>';
  }

  sendBtn.disabled = false;
  qInput.focus();
  chat.scrollTop = chat.scrollHeight;
}

sendBtn.addEventListener('click', doAsk);
qInput.addEventListener('keydown', function(e) { if (e.key === 'Enter') doAsk(); });
document.addEventListener('keydown', function(e) {
  if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
    e.preventDefault();
    qInput.focus();
    qInput.select();
  }
});

/* ── Settings ── */
let settingsLoaded = false;
async function loadSettings() {
  if (settingsLoaded) return;
  settingsLoaded = true;
  loadModels();
  loadKeys();
  loadPasses();
}
async function loadPasses() {
  try {
    const r = await fetch('/api/config');
    if (!r.ok) return;
    const d = await r.json();
    document.getElementById('passes-first').value = d.passes_first || 2;
    document.getElementById('passes-refresh').value = d.passes_refresh || 1;
  } catch(_) {}
}
async function savePasses() {
  const first = parseInt(document.getElementById('passes-first').value, 10);
  const refresh = parseInt(document.getElementById('passes-refresh').value, 10);
  const status = document.getElementById('passes-status');
  try {
    const r = await fetch('/api/config/passes', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({passes_first: first, passes_refresh: refresh})
    });
    const d = await r.json();
    if (d.error) { status.style.color = 'var(--red)'; status.textContent = d.error; return; }
    status.style.color = 'var(--green)';
    status.textContent = 'Saved';
    setTimeout(function() { status.textContent = ''; }, 3000);
  } catch(e) {
    status.style.color = 'var(--red)';
    status.textContent = 'Error: ' + e.message;
  }
}

/* ── Queue badge ── */
async function pollQueue() {
  try {
    const r = await fetch('/api/queue');
    const d = await r.json();
    const badge = document.getElementById('queue-badge');
    const total = d.pending + d.in_flight + d.failed;
    if (total === 0) { badge.style.display = 'none'; return; }
    badge.style.display = '';
    let parts = [];
    if (d.pending > 0) parts.push(d.pending + ' pending');
    if (d.in_flight > 0) parts.push(d.in_flight + ' running');
    if (d.failed > 0) parts.push(d.failed + ' failed');
    badge.textContent = parts.join(' \xB7 ');
  } catch(_) {}
}
setInterval(pollQueue, 3000);
pollQueue();

/* ── Map view ── */
let mapLoaded = false;
async function loadMap() {
  if (mapLoaded) return;
  mapLoaded = true;
  const table = document.getElementById('map-table');
  try {
    const r = await fetch('/api/map');
    const d = await r.json();
    if (!d.length) {
      table.innerHTML = '<tr><td style="color:var(--muted);padding:12px 8px">No data yet. Run <code>indexa deep &lt;path&gt;</code> first.</td></tr>';
      return;
    }
    table.innerHTML = '<thead><tr><th>Category</th><th>Files</th><th>Size</th></tr></thead>';
    const tbody = document.createElement('tbody');
    d.forEach(function(row) {
      const tr = document.createElement('tr');
      const sz = row.total_size > 0 ? (row.total_size > 1048576 ? (row.total_size/1048576).toFixed(1)+' MB' : (row.total_size/1024).toFixed(0)+' KB') : '';
      tr.innerHTML = '<td>' + escapeHtml(row.category || 'Unknown') + '</td><td style="text-align:right">' + (row.entry_count||0).toLocaleString() + '</td><td style="text-align:right">' + sz + '</td>';
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
  } catch(e) {
    table.innerHTML = '<tr><td style="color:var(--red)">' + escapeHtml(e.message) + '</td></tr>';
  }
}

async function loadModels() {
  const list = document.getElementById('models-list');
  try {
    const r = await fetch('/api/models/installed');
    const models = await r.json();
    if (models.error) throw new Error(models.error);
    if (!models.length) {
      list.innerHTML = '<div style="color:var(--muted);font-size:12px">No models installed. Pull one below.</div>';
      return;
    }
    list.innerHTML = models.map(function(m) {
      const mb = m.size > 0 ? (m.size / 1024 / 1024).toFixed(0) + ' MB' : '';
      return '<div class="model-row"><span class="model-name">' + escapeHtml(m.name) + '</span>' +
        '<span class="model-size">' + mb + '</span></div>';
    }).join('');
  } catch(e) {
    list.innerHTML = '<div style="color:var(--red);font-size:12px">Ollama not reachable: ' + escapeHtml(e.message) + '</div>';
  }
}

async function pullModel() {
  const input = document.getElementById('pull-input');
  const name = input.value.trim();
  if (!name) return;
  const btn = document.getElementById('pull-btn');
  const prog = document.getElementById('pull-progress');
  btn.disabled = true;
  prog.style.display = 'block';
  prog.textContent = 'Starting pull for ' + name + '…\n';
  try {
    const r = await fetch('/api/models/pull', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({name: name})
    });
    if (!r.ok) { const d = await r.json(); throw new Error(d.error || 'Failed'); }
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    while (true) {
      const {done, value} = await reader.read();
      if (done) break;
      const lines = dec.decode(value, {stream: true}).split('\n').filter(Boolean);
      lines.forEach(function(line) {
        try {
          const d = JSON.parse(line);
          if (d.status) prog.textContent += d.status + (d.completed ? ' ' + d.completed : '') + '\n';
          prog.scrollTop = prog.scrollHeight;
        } catch(_) {}
      });
    }
    prog.textContent += '✓ Done.\n';
    input.value = '';
    settingsLoaded = false; // force reload on next settings open
    setTimeout(loadModels, 500);
  } catch(e) {
    prog.textContent += '✗ Error: ' + e.message + '\n';
  }
  btn.disabled = false;
}

async function loadKeys() {
  try {
    const r = await fetch('/api/keys');
    if (r.status === 403) {
      document.getElementById('key-gate-notice').style.display = 'block';
      ['openai','anthropic','google'].forEach(function(p) {
        document.getElementById('key-' + p).disabled = true;
        document.querySelector('.key-row:has(#key-' + p + ') .btn-sm').disabled = true;
      });
      return;
    }
    const d = await r.json();
    document.getElementById('badge-openai').textContent = d.openai_set ? '✓ set' : '';
    document.getElementById('badge-anthropic').textContent = d.anthropic_set ? '✓ set' : '';
    document.getElementById('badge-google').textContent = d.google_set ? '✓ set' : '';
  } catch(_) {}
}

async function saveKey(provider) {
  const val = document.getElementById('key-' + provider).value.trim();
  if (!val) return clearKey(provider);
  const r = await fetch('/api/keys', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({provider: provider, key: val})
  });
  const d = await r.json();
  if (d.error) { alert(d.error); return; }
  document.getElementById('key-' + provider).value = '';
  loadKeys();
}

async function clearKey(provider) {
  const r = await fetch('/api/keys', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({provider: provider, key: ''})
  });
  const d = await r.json();
  if (d.error) { alert(d.error); return; }
  loadKeys();
}

/* ── Utilities ── */
function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}
function escapeAttr(s) { return escapeHtml(s); }

/* ── Init ── */
loadStats();
initTree();
switchTab('chat');
reconnectInFlightJobs();
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_html_is_non_empty() {
        assert!(!UI_HTML.is_empty());
        assert!(UI_HTML.contains("Indexa"));
        assert!(UI_HTML.contains("/api/ask"));
    }
}
