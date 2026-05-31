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
    store::Store,
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
);

// ── Public API ────────────────────────────────────────────────────────────────

/// Start the web UI server on `port`. Runs until Ctrl-C or the process exits.
pub async fn serve(
    port: u16,
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
                        .find(|h| *h.status.lock().unwrap() == JobStatus::Running)
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
        .route(
            "/api/config/resource",
            get(api_config_resource_get).post(api_config_resource_set),
        )
        .route("/api/telemetry", get(api_telemetry))
        .route("/api/telemetry/stream", get(api_telemetry_stream))
        .route("/api/models/installed", get(api_models_installed))
        .route("/api/models/pull", post(api_models_pull))
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
