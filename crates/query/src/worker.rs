//! Background summarization worker — drains the summary_queue table.

use crate::summarize::process_queue_item;
use indexa_core::{
    config::DescriberConfig,
    resource::{assess, detect_machine, Pressure, WatchdogState},
    store::Store,
};
use indexa_embed::Embedder;
use indexa_llm::Describer;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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
                // Watchdog: check memory pressure before the LLM call.
                // Hard timeout: max 100 × 3 s = ~5 min to avoid infinite pause.
                let sample = wdog.sample();
                let pressure = assess(&sample, &spec, headroom);
                if pressure != Pressure::Ok {
                    let avail_gb = sample.available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let swap_gb = sample.swap_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    tracing::warn!(
                        "worker: memory pressure — pausing \
                         (available: {avail_gb:.1} GB, swap: {swap_gb:.1} GB)"
                    );
                    let mut ticks = 0u32;
                    loop {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        ticks += 1;
                        if assess(&wdog.sample(), &spec, headroom) == Pressure::Ok {
                            break;
                        }
                        if ticks >= 100 {
                            tracing::warn!(
                                "worker: memory pressure did not clear after 5 min — proceeding"
                            );
                            break;
                        }
                    }
                }

                // Process against the worker's dedicated connection — the shared
                // `store` mutex is never held across this await.
                if let Err(e) = process_queue_item(
                    &mut job_store,
                    describer.as_ref(),
                    embedder.as_ref(),
                    &item,
                    &cfg,
                )
                .await
                {
                    tracing::warn!("worker: process_queue_item error: {e}");
                }
            }
        }
    }
}
