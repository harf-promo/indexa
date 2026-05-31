use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures_util::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::dto::{err_json, EstimateResponse, JobListEntry, JobPathQuery, JobStartResponse};
use crate::jobs::{JobEvent, JobHandle, JobStatus, Jobs};
use crate::jobs_exec::{
    run_deep_phase_standalone, run_index_job, run_scan_phase_standalone, run_summarize_phase,
    schedule_cleanup,
};
use crate::AppState;
use indexa_core::resource::{estimate_eta, fit_report, sample_memory_once};

/// Build a `(file_model, dir_model, num_ctx)` override from job-start query params,
/// when the user picked a model in the "ask me first" popover. Defaults the file
/// model and `num_ctx` so a bare `dir_model=…` is enough.
fn model_override_from(q: &JobPathQuery, default_num_ctx: u32) -> Option<(String, String, u32)> {
    q.dir_model.as_ref().map(|dir| {
        let file = q.file_model.clone().unwrap_or_else(|| dir.clone());
        (file, dir.clone(), q.num_ctx.unwrap_or(default_num_ctx))
    })
}

/// Register a new job in the shared registry and return its handle + id.
pub(crate) async fn register_job(jobs: &Jobs, kind: &str, path: String) -> (Uuid, Arc<JobHandle>) {
    let handle = Arc::new(JobHandle::new(kind, path));
    let id = handle.id;
    jobs.write().await.insert(id, handle.clone());
    (id, handle)
}

pub(crate) async fn api_job_scan(
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

pub(crate) async fn api_job_deep(
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

pub(crate) async fn api_job_summarize(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (id, handle) = register_job(&s.jobs, "summarize", q.path.clone()).await;
    let model_override = model_override_from(&q, s.config.describer.num_ctx);
    let state = s.clone();
    tokio::spawn(async move {
        run_summarize_phase(&state, &q.path, q.passes, &handle, model_override).await;
        schedule_cleanup(state.jobs.clone(), handle.id);
    });
    Json(JobStartResponse { job_id: id })
}

pub(crate) async fn api_job_index(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (id, handle) = register_job(&s.jobs, "index", q.path.clone()).await;
    let model_override = model_override_from(&q, s.config.describer.num_ctx);
    let state = s.clone();
    tokio::spawn(async move {
        let id = handle.id;
        run_index_job(state.clone(), q.path, handle, model_override).await;
        schedule_cleanup(state.jobs.clone(), id);
    });
    Json(JobStartResponse { job_id: id })
}

/// `GET /api/jobs/estimate?path=…` — the pre-flight memory-fit estimate behind the
/// "ask me first" popover. Pure of any filesystem walk: counts come from the store.
pub(crate) async fn api_job_estimate(
    Query(q): Query<JobPathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let cfg = &s.config.describer;
    let headroom = s.config.resource.effective_headroom_bytes();
    let sample = sample_memory_once();
    let report = fit_report(
        &cfg.file_model,
        &cfg.dir_model,
        cfg.num_ctx,
        &s.machine_spec,
        &sample,
        headroom,
    );

    // Approximate counts (global, not scoped to `q.path`) for a rough ETA.
    let (entry_count, chunk_count, queue_pending) = {
        let store = s.store.lock().await;
        (
            store.entry_count().unwrap_or(0),
            store.chunk_count().unwrap_or(0),
            store.queue_stats().map(|qs| qs.pending).unwrap_or(0),
        )
    };
    let passes = q.passes.unwrap_or(2);
    let eta = estimate_eta(
        &cfg.dir_model,
        entry_count as usize,
        chunk_count as usize,
        500, // rough average file token count
        passes,
        s.machine_spec.is_apple_silicon,
    );

    let rec = report.recommended.as_ref();
    Json(EstimateResponse {
        budget_bytes: report.budget_bytes,
        configured_file_model: report.configured.file_model.clone(),
        configured_dir_model: report.configured.dir_model.clone(),
        configured_peak_bytes: report.configured.peak_bytes,
        configured_fits: report.configured.fits,
        recommended_file_model: rec.map(|r| r.file_model.clone()),
        recommended_dir_model: rec.map(|r| r.dir_model.clone()),
        recommended_peak_bytes: rec.map(|r| r.peak_bytes),
        recommended_fits: rec.map(|r| r.fits),
        num_ctx: cfg.num_ctx,
        reason: report.reason.clone(),
        eta_display: eta.display,
        eta_secs: eta.total_secs as u64,
        entry_count,
        chunk_count,
        queue_pending,
    })
}

pub(crate) async fn api_jobs_list(State(s): State<AppState>) -> impl IntoResponse {
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

pub(crate) async fn api_jobs_events(
    Path(id): Path<Uuid>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let handle = match s.jobs.read().await.get(&id) {
        Some(h) => h.clone(),
        None => return (StatusCode::NOT_FOUND, "job not found").into_response(),
    };

    // Subscribe to the live channel FIRST, then snapshot history.  Doing it in this
    // order guarantees no event is lost in the gap: any event pushed between the two
    // statements lands in the live stream.  We dedupe the (small) overlap below by
    // skipping live events whose serialized form already appears at the tail of the
    // replayed history.
    let rx = handle.tx.subscribe();
    let history = handle.history.lock().unwrap().clone();
    // Tail of history used to suppress duplicates that also arrive on the live channel.
    let history_tail: std::collections::HashSet<String> = history
        .iter()
        .rev()
        .take(8)
        .filter_map(|ev| serde_json::to_string(ev).ok())
        .collect();

    fn to_sse(ev: JobEvent) -> Result<Event, Infallible> {
        let data = serde_json::to_string(&ev).unwrap_or_default();
        Ok(Event::default().data(data))
    }

    let replay = futures_util::stream::iter(history).map(to_sse);

    // Capture a handle clone so that on a Lagged drop we can re-deliver the
    // terminal event (Done/Failed), which must never be lost to channel overflow.
    let handle_for_live = handle.clone();
    // Dedup flag: only suppress history/live overlap during the initial window.
    let still_deduping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    let live = BroadcastStream::new(rx)
        .flat_map(move |r| {
            use std::sync::atomic::Ordering;
            let mut out: Vec<JobEvent> = Vec::new();
            match r {
                Ok(ev) => {
                    // Suppress events that already appear in the replayed history tail
                    // (the small subscribe/snapshot overlap), but stop once we see a
                    // fresh event so legitimately-repeated events aren't dropped.
                    if still_deduping.load(Ordering::Relaxed) {
                        let serialized = serde_json::to_string(&ev).unwrap_or_default();
                        if history_tail.contains(&serialized) {
                            return futures_util::stream::iter(out);
                        }
                        still_deduping.store(false, Ordering::Relaxed);
                    }
                    out.push(ev);
                }
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    out.push(JobEvent::Warning {
                        stage: "sse".into(),
                        item_path: None,
                        message: format!("dropped {n} events — resyncing"),
                        pressure: None,
                    });
                    // A terminal event may have been among the dropped ones. Re-deliver
                    // it from history so the client always learns the job finished.
                    let status = handle_for_live.status.lock().unwrap().clone();
                    if status != JobStatus::Running {
                        if let Some(term) = handle_for_live
                            .history
                            .lock()
                            .unwrap()
                            .iter()
                            .rev()
                            .find(|e| matches!(e, JobEvent::Done { .. } | JobEvent::Failed { .. }))
                            .cloned()
                        {
                            out.push(term);
                        }
                    }
                }
            }
            futures_util::stream::iter(out)
        })
        .map(to_sse);

    Sse::new(replay.chain(live))
        .keep_alive(KeepAlive::new())
        .into_response()
}

/// JSON snapshot of a single job (status + last progress event) without SSE.
pub(crate) async fn api_job_get(
    Path(id): Path<Uuid>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let jobs = s.jobs.read().await;
    let Some(h) = jobs.get(&id) else {
        return err_json(StatusCode::NOT_FOUND, "job not found");
    };
    let status = h.status.lock().unwrap().clone();
    let history = h.history.lock().unwrap().clone();
    let last_event = history.last().cloned();
    let resp = serde_json::json!({
        "job_id": h.id,
        "kind": h.kind,
        "path": h.path,
        "started_at": h.started_at,
        "status": status,
        "history": history,
        "last_event": last_event,
    });
    (StatusCode::OK, Json(resp)).into_response()
}

pub(crate) async fn api_job_delete(
    Path(id): Path<Uuid>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    // Request cancellation so the spawned task actually stops its work, rather than
    // continuing to embed/call the LLM invisibly after being removed from the registry.
    // We keep the handle in the registry so the running task can still observe the flag
    // and emit its terminal event; the task's own cleanup removes it, or it ages out.
    let jobs = s.jobs.read().await;
    if let Some(handle) = jobs.get(&id) {
        handle
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    StatusCode::NO_CONTENT
}
