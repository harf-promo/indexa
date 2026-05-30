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
