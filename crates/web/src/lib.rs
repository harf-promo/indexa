//! Local web server — axum-based API + embedded HTML/JS UI.
//!
//! Serves at `http://localhost:<port>` with:
//! - `GET /`             — the single-page UI
//! - `GET /api/stats`    — { entries, chunks }
//! - `GET /api/map`      — [{ category, entry_count, total_size }]
//! - `POST /api/ask`     — { question } → { answer, sources }
//! - `POST /api/ask/stream` — { question } → SSE: `sources` event, then `fragment` events, then `done`
//! - `POST /api/jobs/index?path=` — start scan→deep→summarize job, returns { job_id }
//! - `GET /api/jobs`     — list active jobs
//! - `GET /api/jobs/{id}/events` — SSE progress stream

mod dto;
mod handlers;
mod jobs;
mod jobs_exec;
mod update_control;
mod update_progress;

pub use update_control::{wait_for_command as wait_for_update_command, UpdateCommand};
pub use update_progress::{report_update_progress, UpdateProgress};

use anyhow::Result;
use axum::{
    http::header,
    routing::{delete, get, post},
    Router,
};
use indexa_core::{
    config::Config,
    resource::{detect_machine, MachineSpec, TelemetrySampler},
    store::{AnnIndex, Store},
};
use indexa_embed::Embedder;
use indexa_llm::Generator;
use jobs::{JobStatus, Jobs};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use handlers::*;

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) embedder: Arc<dyn Embedder + Send + Sync + 'static>,
    pub(crate) llm: Arc<dyn Generator + Send + Sync + 'static>,
    pub(crate) config: Arc<Config>,
    pub(crate) jobs: Jobs,
    pub(crate) db_path: Arc<std::path::PathBuf>,
    pub(crate) log_dir: Arc<std::path::PathBuf>,
    /// Limits concurrent filesystem walks to prevent rayon global-pool starvation.
    pub(crate) walk_semaphore: Arc<tokio::sync::Semaphore>,
    /// Detected machine spec (RAM, cores, Apple Silicon flag) — used by the watchdog.
    pub(crate) machine_spec: Arc<MachineSpec>,
    /// Latest machine telemetry (CPU/RAM/swap/pressure), refreshed ~1.5 s by a
    /// background sampler. Read by the `/api/telemetry` endpoints.
    pub(crate) telemetry: tokio::sync::watch::Receiver<dto::TelemetrySample>,
    /// Cached in-memory ANN index for dense retrieval (opt-in, `[retrieval] ann`), lazily
    /// built on first Ask and rebuilt when the chunk watermark changes. See `ensure_ann`.
    pub(crate) ann: Arc<tokio::sync::RwLock<AnnCache>>,
    /// Serializes ANN index builds so N concurrent cold/stale Asks don't each allocate a
    /// full index (each build transiently holds all embeddings — N× would risk OOM).
    pub(crate) ann_build_lock: Arc<tokio::sync::Mutex<()>>,
    /// Active filesystem watch sessions keyed by root path. Each task re-embeds changed
    /// files and marks stale summaries for re-generation, exactly as `indexa watch` does.
    pub(crate) watch_sessions:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, WatchTaskInfo>>>,
}

/// Info about a running watch task so it can be listed and aborted via the web API.
pub(crate) struct WatchTaskInfo {
    /// Abort signal to stop the background watcher task.
    pub(crate) abort: tokio::task::AbortHandle,
    /// Total file-change events processed since the session started.
    pub(crate) events_count: Arc<std::sync::atomic::AtomicU64>,
    /// Unix timestamp when watching started.
    pub(crate) started_at: u64,
}

/// The web server's cached ANN index plus the `(chunk_count, last_indexed_at)` watermark it
/// was built at — a mismatch means `deep` changed the table and the index must be rebuilt.
#[derive(Default)]
pub(crate) struct AnnCache {
    pub(crate) index: Option<Arc<AnnIndex>>,
    pub(crate) watermark: (i64, i64),
}

// ── Embedded UI (split into asset files, included at compile time) ──────────

pub(crate) const UI_HTML: &str = include_str!("../assets/ui/index.html");

/// The Indexa mark (green Harf apostrophe on an ink ground) served as the browser favicon.
pub(crate) const FAVICON_SVG: &str = include_str!("../assets/ui/favicon.svg");

// app.css and app.js are split into ordered source fragments for maintainability
// and reassembled here at compile time. The concat! order below is the canonical
// on-disk order (zero-padded prefixes); the served bytes are byte-identical to the
// pre-split single files. Do not reorder without re-verifying byte-for-byte.
pub(crate) const UI_CSS: &str = concat!(
    include_str!("../assets/ui/css/01-tokens.css"),
    include_str!("../assets/ui/css/02-base.css"),
    include_str!("../assets/ui/css/03-topbar.css"),
    include_str!("../assets/ui/css/04-layout.css"),
    include_str!("../assets/ui/css/05-views.css"),
    include_str!("../assets/ui/css/06-overlays.css"),
    include_str!("../assets/ui/css/07-jobs.css"),
    include_str!("../assets/ui/css/08-engine.css"),
    include_str!("../assets/ui/css/09-model-fit-popover.css"),
    include_str!("../assets/ui/css/10-treemap.css"),
    include_str!("../assets/ui/css/11-graph.css"),
    include_str!("../assets/ui/css/12-review.css"),
    include_str!("../assets/ui/css/13-responsive.css"),
    include_str!("../assets/ui/css/14-update-overlay.css"),
    include_str!("../assets/ui/css/15-file-preview.css"),
    include_str!("../assets/ui/css/16-update-changelog.css"),
    include_str!("../assets/ui/css/17-legible.css"),
    include_str!("../assets/ui/css/18-graph-explore.css"),
    include_str!("../assets/ui/css/20-graph-layers.css"),
    include_str!("../assets/ui/css/19-conversation.css"),
);
pub(crate) const UI_JS: &str = concat!(
    include_str!("../assets/ui/js/01-state-theme-tabs.js"),
    include_str!("../assets/ui/js/02-stats-tree.js"),
    include_str!("../assets/ui/js/03-jobs-search.js"),
    include_str!("../assets/ui/js/04-jobs-views.js"),
    include_str!("../assets/ui/js/05-summary.js"),
    include_str!("../assets/ui/js/06-chat-settings.js"),
    include_str!("../assets/ui/js/07-map.js"),
    include_str!("../assets/ui/js/08-util-palette-init.js"),
    include_str!("../assets/ui/js/09-engine.js"),
    include_str!("../assets/ui/js/10-model-fit-popover.js"),
    include_str!("../assets/ui/js/11-onboarding.js"),
    include_str!("../assets/ui/js/12-treemap.js"),
    include_str!("../assets/ui/js/13-classify.js"),
    include_str!("../assets/ui/js/14-watch.js"),
    include_str!("../assets/ui/js/15-update.js"),
    include_str!("../assets/ui/js/16-context-packs.js"),
    include_str!("../assets/ui/js/17-weights.js"),
    include_str!("../assets/ui/js/18-insights.js"),
    include_str!("../assets/ui/js/19-graph.js"),
    include_str!("../assets/ui/js/20-review.js"),
    include_str!("../assets/ui/js/21-impact.js"),
    include_str!("../assets/ui/js/22-responsive.js"),
    include_str!("../assets/ui/js/23-sidebar-resize.js"),
    include_str!("../assets/ui/js/24-file-preview.js"),
    include_str!("../assets/ui/js/25-graph-explore.js"),
    include_str!("../assets/ui/js/26-url-state.js"),
    include_str!("../assets/ui/js/27-health.js"),
    include_str!("../assets/ui/js/28-graph-layers.js"),
);

// ── Public API ────────────────────────────────────────────────────────────────

/// Start the web UI server on `host:port`. Runs until Ctrl-C or the process exits.
///
/// `host` defaults to `"127.0.0.1"` (localhost-only). Pass `"0.0.0.0"` for LAN access.
/// **Warning:** binding to 0.0.0.0 exposes all indexed files on your local network.
pub async fn serve(
    port: u16,
    host: &str,
    mut store: Store,
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

    // Startup sweep: reset any queue items left `in_flight` by a previous run that crashed
    // or was killed mid-summarize. Safe to do here — no summarize job can be running yet.
    match store.requeue_stale_in_flight(3) {
        Ok((requeued, failed)) if requeued > 0 || failed > 0 => {
            tracing::info!("startup: requeued {requeued} stale in-flight summary items, failed {failed} over the attempt cap");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("startup: failed to sweep stale in-flight queue items: {e}"),
    }

    let config = Arc::new(config);
    let machine_spec = Arc::new(detect_machine());
    let jobs: Jobs = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    // Always-on machine telemetry: sample CPU + memory every ~1.5 s (even when
    // idle) and publish to a watch channel the /api/telemetry endpoints read.
    // Runs on its OWN low-frequency task — never in the per-file job hot loop —
    // so the gauges are never blank and the extra refresh_cpu() cost stays cheap.
    let (telemetry_tx, telemetry_rx) = tokio::sync::watch::channel(dto::TelemetrySample::default());
    {
        let spec = machine_spec.clone();
        let cfg = config.clone();
        let jobs = jobs.clone();
        tokio::spawn(async move {
            let mut sampler = TelemetrySampler::new();
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(1500));
            loop {
                ticker.tick().await;
                let (cpu, mem) = sampler.sample();
                let headroom = cfg.resource.effective_headroom_bytes();
                let active_job = {
                    let map = jobs.read().await;
                    map.values()
                        .find(|h| h.status() == JobStatus::Running)
                        .map(|h| dto::ActiveJobDto {
                            job_id: h.id,
                            kind: h.kind.clone(),
                            path: h.path.clone(),
                        })
                };
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let sample =
                    dto::TelemetrySample::build(&spec, &mem, cpu, headroom, active_job, ts);
                // Ignored: `send` only fails once every receiver has dropped (shutdown).
                let _ = telemetry_tx.send(sample);
            }
        });
    }

    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        embedder,
        llm,
        config,
        jobs,
        db_path,
        log_dir,
        walk_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
        machine_spec,
        telemetry: telemetry_rx,
        ann: Arc::new(tokio::sync::RwLock::new(AnnCache::default())),
        ann_build_lock: Arc::new(tokio::sync::Mutex::new(())),
        watch_sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };

    let app = build_router(state, port);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Indexa web UI listening on http://{addr}");

    if host == "127.0.0.1" || host == "localhost" {
        println!("Open http://localhost:{port} in your browser. Press Ctrl-C to stop.");
    } else {
        // LAN mode: print all non-loopback IPv4 addresses so the user knows what to connect to.
        println!("Open http://localhost:{port} in your browser. Press Ctrl-C to stop.");
        println!("⚠  LAN mode active — also accessible at:");
        if let Ok(ifaces) = if_addrs::get_if_addrs() {
            // crate: if-addrs
            for iface in ifaces {
                let ip = iface.ip();
                if !ip.is_loopback() && ip.is_ipv4() {
                    println!("   http://{}:{port}", ip);
                }
            }
        }
        println!("   Ensure your network is trusted before sharing this URL.");
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the full axum router (all routes + layers) for the given app state.
///
/// `port` is used only to lock the CORS allow-origin to `http://localhost:<port>`.
/// Extracted from [`serve`] so integration tests can drive handlers through the real
/// router (routing + extractors + layers) via `tower::ServiceExt::oneshot`.
pub(crate) fn build_router(state: AppState, port: u16) -> Router {
    // Restrict CORS to localhost only — prevents drive-by sites from reading the
    // user's private index via cross-origin requests to the local server.
    let origin = format!("http://localhost:{port}")
        .parse::<axum::http::HeaderValue>()
        .expect("valid localhost origin header");

    Router::new()
        .route("/", get(serve_ui))
        .route("/assets/app.css", get(serve_ui_css))
        .route("/assets/app.js", get(serve_ui_js))
        .route("/favicon.svg", get(serve_favicon))
        .route("/api/stats", get(api_stats))
        .route("/api/impact", get(api_impact))
        .route("/api/session-impact/{session_id}", get(api_session_impact))
        .route("/api/classifications", get(api_classifications_list))
        .route(
            "/api/classifications/confirm",
            post(api_classifications_confirm),
        )
        .route(
            "/api/classifications/ignore",
            post(api_classifications_ignore),
        )
        .route(
            "/api/classifications/reset",
            post(api_classifications_reset),
        )
        .route("/api/export", get(api_export))
        .route("/api/map", get(api_map))
        .route("/api/map/treemap", get(api_map_treemap))
        .route("/api/graph", get(api_graph))
        .route("/api/roots", get(api_roots))
        .route("/api/search", get(api_search))
        .route("/api/fs/ls", get(api_fs_ls))
        .route("/api/file", get(api_file_preview))
        .route("/api/inspect", get(api_inspect))
        .route("/api/health", get(api_health))
        .route("/api/ask", post(api_ask))
        .route("/api/ask/stream", post(api_ask_stream))
        .route("/api/ask/explain", post(api_ask_explain))
        .route("/api/tree", get(api_tree))
        .route("/api/summary", get(api_summary))
        .route("/api/summarize", post(api_summarize_enqueue))
        .route("/api/queue", get(api_queue_stats))
        .route("/api/queue/failed", get(api_queue_failed))
        .route("/api/queue/retry", post(api_queue_retry))
        .route("/api/config", get(api_config_get))
        .route("/api/config/passes", post(api_config_passes))
        .route("/api/config/provider", post(api_config_provider_set))
        .route(
            "/api/config/resource",
            get(api_config_resource_get).post(api_config_resource_set),
        )
        .route(
            "/api/config/features",
            get(api_config_features_get).post(api_config_features_set),
        )
        .route("/api/telemetry", get(api_telemetry))
        .route("/api/telemetry/stream", get(api_telemetry_stream))
        .route("/api/engine/release", post(api_engine_release))
        .route("/api/engine/processes", get(api_engine_processes))
        .route("/api/models", get(api_models))
        .route("/api/models/installed", get(api_models_installed))
        .route("/api/models/pull", post(api_models_pull))
        .route(
            "/api/models/catalog/refresh",
            post(api_models_catalog_refresh),
        )
        .route("/api/keys", get(api_keys_get).post(api_keys_set))
        .route("/api/providers/status", get(api_providers_status))
        .route("/api/jobs", get(api_jobs_list))
        .route("/api/jobs/scan", post(api_job_scan))
        .route("/api/jobs/deep", post(api_job_deep))
        .route("/api/jobs/summarize", post(api_job_summarize))
        .route("/api/jobs/index", post(api_job_index))
        .route("/api/jobs/estimate", get(api_job_estimate))
        .route("/api/jobs/{id}/events", get(api_jobs_events))
        .route("/api/jobs/{id}", get(api_job_get).delete(api_job_delete))
        .route("/api/entry", delete(api_delete_entry))
        .route("/api/version", get(api_version))
        .route("/api/update/check", get(api_update_check))
        .route("/api/update/apply", post(api_update_apply))
        .route("/api/update/control", post(api_update_control))
        .route(
            "/api/update/progress/stream",
            get(api_update_progress_stream),
        )
        .route("/api/logs/tail", get(api_logs_tail))
        .route("/api/watch/status", get(api_watch_status))
        .route("/api/watch/start", post(api_watch_start))
        .route("/api/watch/stop", post(api_watch_stop))
        .route("/api/packs", get(api_packs_list).post(api_packs_create))
        .route("/api/packs/suggest", post(api_packs_suggest))
        .route("/api/packs/{name}", delete(api_packs_delete))
        .route(
            "/api/packs/{name}/paths",
            get(api_packs_paths_get)
                .post(api_packs_paths_add)
                .delete(api_packs_paths_remove),
        )
        .route("/api/packs/{name}/export", get(api_packs_export))
        .route("/api/packs/{name}/search", get(api_packs_search))
        .route(
            "/api/weights",
            get(api_weights_list)
                .post(api_weights_set)
                .delete(api_weights_delete),
        )
        .route("/api/weights/suggest", get(api_weights_suggest))
        .route("/api/review", get(api_review_list))
        .route("/api/review/answer", post(api_review_answer))
        .route("/api/review/answer-batch", post(api_review_answer_batch))
        .route("/api/review/dismiss", post(api_review_dismiss))
        .route("/api/review/history", get(api_review_history))
        .route("/api/review/revert", post(api_review_revert))
        .route("/api/review/count", get(api_review_count))
        .route(
            "/api/review/dismiss-evidence",
            post(api_review_dismiss_evidence),
        )
        .route("/api/saved", get(api_saved_list).post(api_saved_set))
        .route("/api/saved/{name}", delete(api_saved_delete))
        .route("/api/insights/duplicates", get(api_insights_duplicates))
        .route("/api/insights/stale", get(api_insights_stale))
        .route("/api/insights/diff", get(api_insights_diff))
        .route("/api/insights/largest", get(api_insights_largest))
        .route("/api/insights/languages", get(api_insights_languages))
        .with_state(state)
        // Pin the request-body cap explicitly. axum's built-in default is also 2 MB, but
        // stating it documents the limit and survives a future default change. Every
        // legitimate POST body here (ask question, config, pack path lists, weights) is far
        // under 2 MB, so this rejects oversized bodies before they're buffered into memory.
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::DELETE,
                ])
                .allow_headers([header::CONTENT_TYPE]),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use indexa_core::store::ChunkRecord;
    use indexa_core::walker::{Entry, EntryKind};
    use std::path::PathBuf;
    use tower::ServiceExt; // brings `Router::oneshot`

    #[test]
    fn ui_assets_non_empty() {
        assert!(!UI_HTML.is_empty());
        assert!(UI_HTML.contains("Indexa"));
        assert!(!UI_CSS.is_empty());
        assert!(!UI_JS.is_empty());
        assert!(UI_JS.contains("/api/ask"));
    }

    // ── Test scaffolding ────────────────────────────────────────────────────────
    // Stub AI backends: the handlers exercised below (stats/search/keys) never call
    // them, but `AppState` requires concrete trait objects.
    struct StubEmbedder;
    #[async_trait::async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }
    struct StubGenerator;
    #[async_trait::async_trait]
    impl Generator for StubGenerator {
        async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok("stub".to_owned())
        }
    }

    fn entry(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size: 10,
            modified: None,
            hint: None,
        }
    }
    fn chunk(path: &str, seq: usize, text: &str) -> ChunkRecord {
        ChunkRecord {
            entry_path: path.to_owned(),
            seq,
            heading: String::new(),
            text: text.to_owned(),
            language: None,
            embedding: None,
            embed_model: None,
            content_hash: None,
        }
    }

    fn state_with(store: Store) -> AppState {
        state_with_db(store, PathBuf::from(":memory:"))
    }

    /// A unique temp DB path for tests that need a real file (so a handler's
    /// `Store::open(db_path)` reopens the SAME database — `:memory:` would not).
    fn temp_db_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("indexa-web-test-{}-{}.db", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn state_with_db(store: Store, db_path: PathBuf) -> AppState {
        // The sender is dropped immediately; the receiver still yields its last value on
        // `borrow()`, and none of the tested handlers read telemetry anyway.
        let (_tx, telemetry) = tokio::sync::watch::channel(crate::dto::TelemetrySample::default());
        AppState {
            store: Arc::new(Mutex::new(store)),
            embedder: Arc::new(StubEmbedder),
            llm: Arc::new(StubGenerator),
            config: Arc::new(Config::default()),
            jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            db_path: Arc::new(db_path),
            log_dir: Arc::new(std::env::temp_dir()),
            walk_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
            machine_spec: Arc::new(indexa_core::resource::detect_machine()),
            telemetry,
            ann: Arc::new(tokio::sync::RwLock::new(AnnCache::default())),
            ann_build_lock: Arc::new(tokio::sync::Mutex::new(())),
            watch_sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// GET `uri` through the real router; return (status, parsed-JSON-body).
    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    // ── Tests ────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn api_stats_empty_store_is_zero() {
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = get_json(app, "/api/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["entries"], 0);
        assert_eq!(json["chunks"], 0);
    }

    #[tokio::test]
    async fn api_stats_counts_seeded_rows() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                entry("/r", EntryKind::Dir),
                entry("/r/a.rs", EntryKind::File),
            ])
            .unwrap();
        store
            .upsert_chunks(&[chunk("/r/a.rs", 0, "fn main() {}")])
            .unwrap();
        let app = build_router(state_with(store), 7620);
        let (status, json) = get_json(app, "/api/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["entries"], 2);
        assert_eq!(json["chunks"], 1);
    }

    #[tokio::test]
    async fn api_search_empty_query_returns_empty_array() {
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = get_json(app, "/api/search?q=").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn api_search_query_returns_ok_array() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[entry("/r/alpha.rs", EntryKind::File)])
            .unwrap();
        store
            .upsert_chunks(&[chunk("/r/alpha.rs", 0, "the quick brown fox")])
            .unwrap();
        let app = build_router(state_with(store), 7620);
        let (status, json) = get_json(app, "/api/search?q=alpha").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.is_array());
    }

    #[tokio::test]
    async fn api_review_lists_and_answers_open_decisions() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .record_decision(indexa_core::store::NewDecision {
                decision_type: "classification".to_owned(),
                subject: "/r/proj".to_owned(),
                params: serde_json::json!({"category": "code", "confidence": 0.7}),
                options: serde_json::json!(["work", "code", "ignore"]),
                auto_value: Some("code".to_owned()),
                confidence: Some(0.7),
                evidence_hash: "fp1".to_owned(),
                priority: 50,
                paths: vec!["/r/proj".to_owned()],
            })
            .unwrap()
            .unwrap();
        let state = state_with(store);
        let app = build_router(state.clone(), 7620);

        let (status, json) = get_json(app.clone(), "/api/review").await;
        assert_eq!(status, StatusCode::OK);
        let q = &json.as_array().unwrap()[0];
        assert_eq!(q["decision_type"], "classification");
        assert!(q["title"].as_str().unwrap().contains("/r/proj"));
        let id = q["id"].as_i64().unwrap();

        let (status, json) = get_json(app.clone(), "/api/review/count").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["open"], 1);

        // Answer through the real route: the projection must land in classifications.
        let body = serde_json::json!({ "id": id, "chosen": "work" });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/review/answer")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["answered"], true);
        assert_eq!(json["effects"]["classification"], "work");

        {
            let store = state.store.lock().await;
            let c = store.classification_for("/r/proj").unwrap().unwrap();
            assert_eq!((c.category.as_str(), c.source.as_str()), ("work", "user"));
            assert_eq!(store.open_decision_count().unwrap(), 0);
        }
        let (status, json) = get_json(app, "/api/review/history?subject=/r/proj").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["status"], "decided");
        assert_eq!(json[0]["chosen"], "work");
    }

    #[tokio::test]
    async fn api_review_revert_restores_a_superseded_answer() {
        // Build a chain: decided "work" → re-ask decided "code" (supersedes it),
        // then revert the original via the real route. The endpoint shares
        // core::decisions::revert_decision with the CLI.
        let mut store = Store::open_in_memory().unwrap();
        let q = |hash: &str, priority: i64| indexa_core::store::NewDecision {
            decision_type: "classification".to_owned(),
            subject: "/r/proj".to_owned(),
            params: serde_json::json!({"category": "code", "confidence": 0.7}),
            options: serde_json::json!(["work", "code", "ignore"]),
            auto_value: Some("code".to_owned()),
            confidence: Some(0.7),
            evidence_hash: hash.to_owned(),
            priority,
            paths: vec!["/r/proj".to_owned()],
        };
        let p = store.record_decision(q("fp1", 50)).unwrap().unwrap();
        indexa_core::decisions::decide_and_apply(&mut store, p, "work", "user").unwrap();
        let c = store.supersede_with(p, q("fp2", 100)).unwrap().unwrap();
        indexa_core::decisions::decide_and_apply(&mut store, c, "code", "user").unwrap();
        let state = state_with(store);
        let app = build_router(state.clone(), 7620);

        let body = serde_json::json!({ "id": p });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/review/revert")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["reverted"], true);
        assert_eq!(json["chosen"], "work");
        assert_eq!(json["superseded_id"], c);
        assert_eq!(json["effects"]["classification"], "work");
        {
            let store = state.store.lock().await;
            let cls = store.classification_for("/r/proj").unwrap().unwrap();
            assert_eq!(cls.category, "work");
        }

        // The history route (the chain view's data source) shows 3 revisions.
        let (status, json) = get_json(app, "/api/review/history?subject=/r/proj").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json.as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn api_review_dismiss_evidence_records_sticky_dismissal() {
        // The endpoint re-derives the cluster from the detector's own scan, so
        // the duplicate evidence must really exist (shared content hash).
        let mut seeded = Store::open_in_memory().unwrap();
        for p in ["/r/a.txt", "/r/b.txt"] {
            seeded
                .upsert_summary(&indexa_core::store::SummaryRecord {
                    path: p.to_owned(),
                    kind: "file".into(),
                    parent_path: Some("/r".into()),
                    depth: 2,
                    summary: format!("summary of {p}"),
                    summary_l0: None,
                    embedding: None,
                    child_count: 0,
                    byte_size: 10,
                    model: "test".into(),
                    source_hash: "H1".into(),
                    generated_at: 1,
                })
                .unwrap();
        }
        let state = state_with(seeded);
        let app = build_router(state.clone(), 7620);

        async fn post_dismiss(
            app: Router,
            body: serde_json::Value,
        ) -> (StatusCode, serde_json::Value) {
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/review/dismiss-evidence")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            )
        }

        let body = serde_json::json!({
            "kind": "duplicate",
            "paths": ["/r/a.txt", "/r/b.txt"],
        });
        let (status, json) = post_dismiss(app.clone(), body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["dismissed"], 1);

        // The dismissal never surfaces as an open question.
        {
            let store = state.store.lock().await;
            assert_eq!(store.open_decision_count().unwrap(), 0);
            let hist = store.decision_history("duplicate", "/r/a.txt").unwrap();
            assert_eq!(hist.len(), 1);
            assert_eq!(hist[0].status, "dismissed");
        }

        // Client-input shaped errors are 400s, never 500s.
        let (status, _) = post_dismiss(
            app.clone(),
            serde_json::json!({"kind": "nonsense", "paths": ["/x"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post_dismiss(
            app,
            serde_json::json!({"kind": "duplicate", "paths": ["/only-one"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn api_keys_post_is_forbidden_without_env_gate() {
        // The write gate must reject when INDEXA_WEB_ALLOW_KEY_EDIT != "1". CI leaves it
        // unset and no test in this binary sets it, so the closed-gate path is deterministic.
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let body = serde_json::json!({ "provider": "openai", "key": "sk-test" });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/keys")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_packs_export_redacts_secrets() {
        // Regression guard: the pack export route must scrub secrets before content
        // leaves the machine over HTTP — the same invariant the whole-tree export,
        // MCP export_pack, and CLI `pack export` enforce. A summary carrying an AWS
        // key must come back redacted, never verbatim.
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&indexa_core::store::SummaryRecord {
                path: "/r/creds.txt".into(),
                kind: "file".into(),
                parent_path: Some("/r".into()),
                depth: 2,
                summary: "deploy config: aws_key = AKIAIOSFODNN7EXAMPLE".into(),
                summary_l0: None,
                embedding: None,
                child_count: 0,
                byte_size: 10,
                model: "test".into(),
                source_hash: "H1".into(),
                generated_at: 1,
            })
            .unwrap();
        let pack_id = store.create_pack("secrets", None).unwrap();
        store
            .add_pack_paths(&pack_id, &["/r/creds.txt".to_owned()])
            .unwrap();

        let app = build_router(state_with(store), 7620);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/packs/secrets/export?format=md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !body.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key leaked through pack export: {body}"
        );
        assert!(
            body.contains("[REDACTED-aws-key]"),
            "expected redaction marker in pack export, got: {body}"
        );
    }

    // ── WS8 additions: ask scope/agentic/empty, export empty/depth, stats summaries,
    //    review batch, and the new /api/impact telemetry endpoint ────────────────────

    /// POST `uri` with a JSON body through the real router; return (status, JSON body).
    async fn post_json(
        app: Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn summary(
        path: &str,
        kind: &str,
        parent: Option<&str>,
        depth: i64,
    ) -> indexa_core::store::SummaryRecord {
        indexa_core::store::SummaryRecord {
            path: path.to_owned(),
            kind: kind.to_owned(),
            parent_path: parent.map(str::to_owned),
            depth,
            summary: format!("Summary of {path}."),
            summary_l0: None,
            embedding: None,
            child_count: 0,
            byte_size: 100,
            model: "test".to_owned(),
            source_hash: String::new(),
            generated_at: 0,
        }
    }

    #[tokio::test]
    async fn api_impact_empty_store_has_no_usage() {
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = get_json(app, "/api/impact").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["calls"], 0);
        assert!(json["by_tool"].as_array().unwrap().is_empty());
        // No usage ⇒ no savings sentence (matches UsageSummary::savings_line → None).
        assert!(json["savings_line"].is_null());
    }

    #[tokio::test]
    async fn api_impact_reports_per_tool_breakdown_most_saving_first() {
        let mut store = Store::open_in_memory().unwrap();
        // counterfactual > served so there is savings to report; ask saves more than search.
        store
            .record_tool_usage("web", "ask", 100, 4000, None)
            .unwrap();
        store
            .record_tool_usage("mcp", "search", 50, 2000, None)
            .unwrap();
        store
            .record_tool_usage("web", "ask", 100, 4000, None)
            .unwrap();
        let app = build_router(state_with(store), 7620);
        let (status, json) = get_json(app, "/api/impact").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["calls"], 3);
        let by_tool = json["by_tool"].as_array().unwrap();
        assert_eq!(by_tool.len(), 2);
        // Ordered by avoided bytes desc: ask (2×3900) outranks search (1950).
        assert_eq!(by_tool[0]["tool"], "ask");
        assert_eq!(by_tool[0]["calls"], 2);
        assert!(json["savings_line"]
            .as_str()
            .unwrap()
            .contains("tokens saved"));
    }

    #[tokio::test]
    async fn api_ask_empty_index_answers_without_error() {
        // No chunks ⇒ the pipeline short-circuits with a graceful answer (Ok, not 500)
        // and zero sources. Exercises the buffered handler with the stub backends.
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = post_json(
            app,
            "/api/ask",
            serde_json::json!({ "question": "where is auth?" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["answer"].is_string());
        assert!(json["sources"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_ask_accepts_scope_and_agentic_flags() {
        // The v0.27 scope seam and v0.20 agentic flag must both be accepted and reach a
        // successful (empty-index) answer rather than a deserialize/500 error.
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = post_json(
            app,
            "/api/ask",
            serde_json::json!({ "question": "what is here?", "scope": "/r/sub", "agentic": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["answer"].is_string());
    }

    #[tokio::test]
    async fn api_ask_with_session_id_persists_turns() {
        // Conversational Ask: two POSTs with the same session_id record two turns; the
        // response echoes the id. Needs a file-backed DB so the handler's own connection
        // reopens the same database the AppState store points at.
        let db = temp_db_path("session-roundtrip");
        let store = Store::open(&db).unwrap();
        let app = || build_router(state_with_db(Store::open(&db).unwrap(), db.clone()), 7620);

        let (s1, j1) = post_json(
            app(),
            "/api/ask",
            serde_json::json!({ "question": "first?", "session_id": "sess-1" }),
        )
        .await;
        assert_eq!(s1, StatusCode::OK);
        assert_eq!(j1["session_id"], "sess-1", "response echoes the session id");

        let (s2, _j2) = post_json(
            app(),
            "/api/ask",
            serde_json::json!({ "question": "second?", "session_id": "sess-1" }),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);

        let turns = store.turns_for_session("sess-1").unwrap();
        assert_eq!(turns.len(), 2, "both turns persisted under the session");
        assert_eq!(turns[0].question, "first?");
        assert_eq!(turns[1].question, "second?");
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn api_ask_without_session_id_creates_no_session() {
        // Backward-compat: a stateless ask records no conversation rows and echoes no id.
        let db = temp_db_path("no-session");
        let store = Store::open(&db).unwrap();
        let app = build_router(state_with_db(Store::open(&db).unwrap(), db.clone()), 7620);
        let (status, json) = post_json(
            app,
            "/api/ask",
            serde_json::json!({ "question": "stateless?" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.get("session_id").is_none() || json["session_id"].is_null());
        assert!(store.turns_for_session("anything").unwrap().is_empty());
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn api_export_empty_index_is_not_found() {
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = get_json(app, "/api/export").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json["error"].as_str().unwrap_or("").contains("summarize"));
    }

    #[tokio::test]
    async fn api_export_renders_seeded_summary_respecting_depth() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                entry("/r", EntryKind::Dir),
                entry("/r/a.rs", EntryKind::File),
            ])
            .unwrap();
        store
            .upsert_summary(&summary("/r", "dir", None, 0))
            .unwrap();
        store
            .upsert_summary(&summary("/r/a.rs", "file", Some("/r"), 1))
            .unwrap();
        let app = build_router(state_with(store), 7620);
        // depth=0 ⇒ root summary only; format defaults to XML.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/export?path=/r&depth=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(ct.contains("xml"), "default export format is XML, got {ct}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Summary of /r"));
    }

    #[tokio::test]
    async fn api_stats_counts_summaries() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&summary("/r", "dir", None, 0))
            .unwrap();
        let app = build_router(state_with(store), 7620);
        let (status, json) = get_json(app, "/api/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["summaries"], 1);
    }

    #[tokio::test]
    async fn api_review_answer_batch_answers_all_under_prefix() {
        let mut store = Store::open_in_memory().unwrap();
        let mk = |subject: &str, hash: &str| indexa_core::store::NewDecision {
            decision_type: "classification".to_owned(),
            subject: subject.to_owned(),
            params: serde_json::json!({"category": "code", "confidence": 0.7}),
            options: serde_json::json!(["work", "code", "ignore"]),
            auto_value: Some("code".to_owned()),
            confidence: Some(0.7),
            evidence_hash: hash.to_owned(),
            priority: 50,
            paths: vec![subject.to_owned()],
        };
        store.record_decision(mk("/r/a", "h1")).unwrap().unwrap();
        store.record_decision(mk("/r/b", "h2")).unwrap().unwrap();
        let state = state_with(store);
        let app = build_router(state.clone(), 7620);

        // "work" is a batch-safe classification category (batch_answer_refusal allows it).
        let (status, json) = post_json(
            app,
            "/api/review/answer-batch",
            serde_json::json!({ "type": "classification", "under": "/r", "chosen": "work" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["answered"], 2);
        assert_eq!(json["applied"], 2);
        {
            let store = state.store.lock().await;
            assert_eq!(store.open_decision_count().unwrap(), 0);
            assert_eq!(
                store.classification_for("/r/a").unwrap().unwrap().category,
                "work"
            );
        }
    }

    #[tokio::test]
    async fn api_review_answer_batch_rejects_unsafe_choice() {
        // A canonical-pick answer is per-cluster, never batch-safe — the handler must 400
        // before touching the store (shares batch_answer_refusal with the CLI).
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, _json) = post_json(
            app,
            "/api/review/answer-batch",
            serde_json::json!({ "type": "duplicate", "under": "/r", "chosen": "keep_this_one" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn update_progress_serializes_phase_title_and_bytes() {
        let p = UpdateProgress::downloading("Indexa 9.9.9", 12, Some(34));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["phase"], "downloading");
        assert_eq!(v["title"], "Indexa 9.9.9");
        assert_eq!(v["downloaded"], 12);
        assert_eq!(v["total"], 34);
        // The v0.34 changelog fields are null except on the `available` phase.
        assert_eq!(v["notes"], serde_json::Value::Null);
        assert_eq!(v["version"], serde_json::Value::Null);

        let e = UpdateProgress::error("X", "boom");
        let ev = serde_json::to_value(&e).unwrap();
        assert_eq!(ev["phase"], "error");
        assert_eq!(ev["error"], "boom");

        // `available` carries the full changelog + version for the in-app modal.
        let a = UpdateProgress::available("0.34.0", Some("- New update window".to_owned()));
        let av = serde_json::to_value(&a).unwrap();
        assert_eq!(av["phase"], "available");
        assert_eq!(av["version"], "0.34.0");
        assert_eq!(av["notes"], "- New update window");
        assert_eq!(av["title"], "Indexa 0.34.0");
    }

    #[tokio::test]
    async fn api_file_serves_text_within_roots_and_blocks_outside() {
        // A real temp file inside an indexed root must be previewable; a real file outside any
        // indexed root must be refused (path-confinement, mirroring MCP read_file).
        let base = std::env::temp_dir().canonicalize().unwrap();
        let pid = std::process::id();
        let root = base.join(format!("indexa_fp_root_{pid}"));
        let outside = base.join(format!("indexa_fp_out_{pid}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let infile = root.join("a.rs");
        std::fs::write(&infile, "fn main() { let x = 1; }\n").unwrap();
        let outfile = outside.join("b.rs");
        std::fs::write(&outfile, "secret\n").unwrap();

        let mut store = Store::open_in_memory().unwrap();
        // Mirror a real scan: the root dir is indexed alongside the file under it, so
        // root_paths() yields `root` and the file is confined within it.
        store
            .upsert_entries(&[
                entry(root.to_str().unwrap(), EntryKind::Dir),
                entry(infile.to_str().unwrap(), EntryKind::File),
            ])
            .unwrap();
        let state = state_with(store);

        let (status, json) = get_json(
            build_router(state.clone(), 7620),
            &format!("/api/file?path={}", infile.display()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["language"], "rust");
        assert_eq!(json["binary"], false);
        assert!(json["content"].as_str().unwrap().contains("fn main"));

        let (status_out, _json) = get_json(
            build_router(state, 7620),
            &format!("/api/file?path={}", outfile.display()),
        )
        .await;
        assert_eq!(status_out, StatusCode::FORBIDDEN);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn api_inspect_reports_facts_or_404() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[entry("/r/a.rs", EntryKind::File)])
            .unwrap();
        store
            .upsert_chunks(&[chunk("/r/a.rs", 0, "fn main() {}")])
            .unwrap();
        let app = build_router(state_with(store), 7620);

        let (status, json) = get_json(app.clone(), "/api/inspect?path=/r/a.rs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["kind"], "file");
        assert_eq!(json["chunk_count"], 1);
        assert_eq!(json["has_summary"], false);

        let (status_missing, _json) = get_json(app, "/api/inspect?path=/r/nope.rs").await;
        assert_eq!(status_missing, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_ask_explain_returns_stage_shaped_json() {
        // Smoke: the explain endpoint is wired and returns the trace shape. (The handler opens its
        // own db from db_path = ":memory:" → empty, so stages have no hits, but the shape is fixed.)
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, json) = post_json(
            app,
            "/api/ask/explain",
            serde_json::json!({ "question": "where is auth?" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["stages"].is_array(), "stages array present");
        assert!(json["mode"].is_string(), "mode present");
    }

    #[tokio::test]
    async fn update_control_is_desktop_gated() {
        // Outside the desktop app (INDEXA_DESKTOP unset — the test default), the control endpoint
        // refuses with 403: there's no updater task listening under plain `indexa serve`.
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let (status, _json) = post_json(
            app,
            "/api/update/control",
            serde_json::json!({ "action": "start" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn update_progress_stream_emits_reported_value() {
        use futures_util::StreamExt;

        // Publish BEFORE subscribing — WatchStream yields the latest value to a new subscriber,
        // so the first SSE frame carries it (no startup gap).
        report_update_progress(UpdateProgress::downloading("Indexa test", 50, Some(100)));
        let app = build_router(state_with(Store::open_in_memory().unwrap()), 7620);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/update/progress/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(ct.starts_with("text/event-stream"), "content-type was {ct}");

        let mut stream = resp.into_body().into_data_stream();
        let first = stream.next().await.expect("an SSE frame").unwrap();
        let text = String::from_utf8_lossy(&first);
        assert!(text.contains("\"phase\":\"downloading\""), "frame: {text}");
        assert!(text.contains("\"downloaded\":50"), "frame: {text}");

        // Reset the process-global channel so it doesn't leak a stale value to other tests.
        report_update_progress(UpdateProgress::idle());
    }
}
