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
//! - `GET /api/jobs/:id/events` — SSE progress stream

mod dto;
mod handlers;
mod jobs;
mod jobs_exec;

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
        .route("/api/stats", get(api_stats))
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
        .route("/api/ask", post(api_ask))
        .route("/api/ask/stream", post(api_ask_stream))
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
        .route("/api/jobs/:id/events", get(api_jobs_events))
        .route("/api/jobs/:id", get(api_job_get).delete(api_job_delete))
        .route("/api/entry", delete(api_delete_entry))
        .route("/api/version", get(api_version))
        .route("/api/update/check", get(api_update_check))
        .route("/api/update/apply", post(api_update_apply))
        .route("/api/logs/tail", get(api_logs_tail))
        .route("/api/watch/status", get(api_watch_status))
        .route("/api/watch/start", post(api_watch_start))
        .route("/api/watch/stop", post(api_watch_stop))
        .route("/api/packs", get(api_packs_list).post(api_packs_create))
        .route("/api/packs/suggest", post(api_packs_suggest))
        .route("/api/packs/:name", delete(api_packs_delete))
        .route(
            "/api/packs/:name/paths",
            get(api_packs_paths_get)
                .post(api_packs_paths_add)
                .delete(api_packs_paths_remove),
        )
        .route("/api/packs/:name/export", get(api_packs_export))
        .route("/api/packs/:name/search", get(api_packs_search))
        .route(
            "/api/weights",
            get(api_weights_list)
                .post(api_weights_set)
                .delete(api_weights_delete),
        )
        .route("/api/weights/suggest", get(api_weights_suggest))
        .route("/api/insights/duplicates", get(api_insights_duplicates))
        .route("/api/insights/stale", get(api_insights_stale))
        .route("/api/insights/diff", get(api_insights_diff))
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
        }
    }

    fn state_with(store: Store) -> AppState {
        // The sender is dropped immediately; the receiver still yields its last value on
        // `borrow()`, and none of the tested handlers read telemetry anyway.
        let (_tx, telemetry) = tokio::sync::watch::channel(crate::dto::TelemetrySample::default());
        AppState {
            store: Arc::new(Mutex::new(store)),
            embedder: Arc::new(StubEmbedder),
            llm: Arc::new(StubGenerator),
            config: Arc::new(Config::default()),
            jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            db_path: Arc::new(PathBuf::from(":memory:")),
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
}
