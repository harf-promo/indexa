//! Always-on machine telemetry endpoints.
//!
//! A background sampler (spawned in `serve()`) publishes a `TelemetrySample`
//! every ~1.5 s to a `watch` channel on `AppState`. These two handlers expose it:
//! a one-shot poll and an SSE stream. Both serve the same latest-value snapshot,
//! and the sampler runs even when no job is active, so the UI's CPU/RAM/pressure
//! gauges are live whether the engine is idle or building.

use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use std::convert::Infallible;
use tokio_stream::{wrappers::WatchStream, StreamExt};

use crate::AppState;

/// `POST /api/engine/release` — unload the local models Indexa loaded, freeing
/// their wired RAM. This frees ONLY Indexa's own footprint: the Ollama models it
/// keeps resident (`keep_alive`) for fast follow-up calls. It is **not** a system
/// memory purge — Indexa does not (and should not) touch the OS's reclaimable
/// cache. Cloud providers hold no local model, so their `unload()` is a no-op.
/// The memory watchdog already does this automatically under pressure; this is
/// the manual lever for "give my RAM back now".
pub(crate) async fn api_engine_release(State(s): State<AppState>) -> impl IntoResponse {
    // Best-effort: unload never errors out of the trait; Ollama logs+ignores a
    // failed evict. Models free asynchronously, so the streamed engine bar shows
    // `used` falling / `budget` climbing a moment later.
    s.llm.unload().await;
    s.embedder.unload().await;
    Json(serde_json::json!({ "released": true }))
}

/// `GET /api/engine/processes` — the top memory-consuming processes system-wide, so the user
/// can decide what to quit to free RAM. **Read-only**: Indexa reports, it never kills or purges
/// (an app can't safely reclaim another process's memory, and a system cache purge is
/// counterproductive — see the engine-bar help). Heavier than the telemetry sample (a full
/// process refresh), so it's a manual, on-demand poll, off the runtime via `spawn_blocking`.
pub(crate) async fn api_engine_processes() -> impl IntoResponse {
    let procs = tokio::task::spawn_blocking(|| indexa_core::resource::top_memory_consumers(12))
        .await
        .unwrap_or_default();
    let arr: Vec<_> = procs
        .into_iter()
        .map(|p| serde_json::json!({ "pid": p.pid, "name": p.name, "rss_bytes": p.rss_bytes }))
        .collect();
    Json(serde_json::json!({ "processes": arr }))
}

/// `GET /api/telemetry` — the latest telemetry snapshot (poll-friendly fallback).
pub(crate) async fn api_telemetry(State(s): State<AppState>) -> impl IntoResponse {
    let sample = s.telemetry.borrow().clone();
    Json(sample)
}

/// `GET /api/telemetry/stream` — SSE that emits the current sample immediately,
/// then on every subsequent update. `WatchStream` yields the latest value to each
/// new subscriber, so there is no startup gap and no replay/dedup machinery needed.
pub(crate) async fn api_telemetry_stream(State(s): State<AppState>) -> impl IntoResponse {
    let stream = WatchStream::new(s.telemetry.clone()).map(|sample| {
        let data = serde_json::to_string(&sample).unwrap_or_default();
        Ok::<Event, Infallible>(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::new())
}
