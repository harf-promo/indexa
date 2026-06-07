//! Background summarization worker — drains the summary_queue table.

use crate::summarize::{process_queue_item, QueueOutcome};
use indexa_core::{
    config::DescriberConfig,
    resource::{assess, detect_machine, pause_step, PauseAction, Pressure, WatchdogState},
    store::Store,
};
use indexa_embed::Embedder;
use indexa_llm::Describer;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::MAX_DIR_DEFERS;

/// Run the background summarization worker until the channel is closed or the
/// process exits. Items are processed one at a time per worker instance;
/// launch multiple tasks for `cfg.queue_concurrency > 1`.
///
/// `headroom_bytes` is the memory headroom the watchdog keeps free (from the
/// user's `[resource]` config). Pass 0 to fall back to a conservative 4 GB.
pub async fn run_worker(
    store: Arc<Mutex<Store>>,
    describer: Arc<dyn Describer + Send + Sync>,
    embedder: Arc<dyn Embedder + Send + Sync>,
    cfg: DescriberConfig,
    headroom_bytes: u64,
) {
    // Detect machine spec once for the watchdog.
    let spec = detect_machine();
    let headroom = if headroom_bytes > 0 {
        headroom_bytes
    } else {
        4 * 1024 * 1024 * 1024_u64
    };
    let mut wdog = WatchdogState::new();

    // Open a dedicated Store connection owned by this worker so the LLM call below
    // never holds the shared mutex across an await — that would block every other
    // reader (e.g. web UI handlers) for the full duration of the LLM round-trip.
    let db_path = store.lock().await.db_path().to_path_buf();
    let mut job_store = match Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("worker: failed to open dedicated store connection: {e}");
            return;
        }
    };

    // Per-loop count of how many times each directory has been deferred (children not
    // yet summarized), to apply the force-rollup cap. Pruned on a terminal outcome.
    let mut defers: HashMap<String, u32> = HashMap::new();

    loop {
        let item = match job_store.next_queue_item() {
            Ok(item) => item,
            Err(e) => {
                tracing::warn!("worker: queue poll error: {e}");
                None
            }
        };

        match item {
            None => {
                // Nothing pending — sleep briefly and poll again
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Some(item) => {
                // Watchdog: check memory pressure before the LLM call. The pause loop uses
                // the shared, recover-aware `pause_step` so it agrees with the web path: it
                // resumes the moment free RAM climbs back above headroom (macOS swap is sticky
                // and never drains on its own), capped at resource::MAX_PAUSE_SECS.
                // Gate entry on the same recover-aware predicate as resume, not raw `assess()`:
                // macOS swap is sticky, so `assess()` reports Critical for the rest of the job
                // once it crosses the threshold even after RAM recovers — which would re-pause
                // (and reload the model) on every item. `pause_step(.., 0) != Resume` means RAM
                // is genuinely low.
                let sample = wdog.sample();
                if pause_step(&spec, &sample, headroom, 0) != PauseAction::Resume {
                    let pressure = assess(&sample, &spec, headroom);
                    let avail_gb = sample.available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let swap_gb = sample.swap_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    tracing::warn!(
                        "worker: low on memory — easing off and freeing the model \
                         (available: {avail_gb:.1} GB, swap: {swap_gb:.1} GB)"
                    );
                    // On a Critical entry, unload the resident models once so their wired RAM
                    // frees and the recovery check can resume us.
                    if pressure == Pressure::Critical {
                        embedder.unload().await;
                        describer.unload().await;
                    }
                    let mut elapsed = 0u64;
                    loop {
                        match pause_step(&spec, &wdog.sample(), headroom, elapsed) {
                            PauseAction::Resume => break,
                            PauseAction::Proceed => {
                                tracing::warn!(
                                    "worker: memory didn't recover after {}s — continuing gently",
                                    indexa_core::resource::MAX_PAUSE_SECS
                                );
                                break;
                            }
                            PauseAction::Sleep(secs) => {
                                tokio::time::sleep(Duration::from_secs(secs)).await;
                                elapsed += secs;
                            }
                        }
                    }
                }

                // Force the roll-up once this dir has been deferred too many times, so a
                // stuck/hung child can't block it forever.
                let force = item.kind == "dir"
                    && defers.get(&item.path).copied().unwrap_or(0) >= MAX_DIR_DEFERS;

                // Process against the worker's dedicated connection — the shared
                // `store` mutex is never held across this await.
                match process_queue_item(
                    &mut job_store,
                    describer.as_ref(),
                    embedder.as_ref(),
                    &item,
                    &cfg,
                    force,
                )
                .await
                {
                    Ok(QueueOutcome::Deferred) => {
                        // Children not summarized yet; the dir was re-enqueued `pending`.
                        // Back off briefly so we don't hot-spin while a sibling worker
                        // finishes them, then poll again.
                        *defers.entry(item.path.clone()).or_insert(0) += 1;
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Ok(_) => {
                        defers.remove(&item.path);
                    }
                    Err(e) => {
                        // `process_queue_item` only returns Err on an unexpected store error,
                        // which leaves the claimed row `in_flight`. Terminalize it (best-effort)
                        // so it can't get stuck blocking the queue until the next restart sweep.
                        defers.remove(&item.path);
                        tracing::warn!("worker: process_queue_item error for {}: {e}", item.path);
                        if let Err(e2) = job_store.mark_queue_state(
                            &item.path,
                            "failed",
                            Some(&format!("{e:#}")),
                        ) {
                            tracing::warn!("worker: could not terminalize {}: {e2}", item.path);
                        }
                    }
                }
            }
        }
    }
}
