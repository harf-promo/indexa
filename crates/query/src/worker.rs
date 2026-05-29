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
pub async fn run_worker(
    store: Arc<Mutex<Store>>,
    describer: Arc<dyn Describer + Send + Sync>,
    embedder: Arc<dyn Embedder + Send + Sync>,
    cfg: DescriberConfig,
) {
    // Detect machine spec once for the watchdog.
    let spec = detect_machine();
    // Use a conservative 4 GB headroom for the CLI worker (no resource config available here).
    let headroom = 4 * 1024 * 1024 * 1024_u64;
    let mut wdog = WatchdogState::new();

    loop {
        let item = {
            let mut s = store.lock().await;
            match s.next_queue_item() {
                Ok(item) => item,
                Err(e) => {
                    tracing::warn!("worker: queue poll error: {e}");
                    None
                }
            }
        };

        match item {
            None => {
                // Nothing pending — sleep briefly and poll again
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Some(item) => {
                // Watchdog: check memory pressure before the LLM call.
                let sample = wdog.sample();
                let pressure = assess(&sample, &spec, headroom);
                if pressure != Pressure::Ok {
                    let level = if pressure == Pressure::Critical {
                        "critical"
                    } else {
                        "high"
                    };
                    let free_gb = sample.free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let swap_gb = sample.swap_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    tracing::warn!(
                        "worker: memory pressure {level} — pausing \
                         (free: {free_gb:.1} GB, swap: {swap_gb:.1} GB)"
                    );
                    // Pause until pressure clears.
                    loop {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        if assess(&wdog.sample(), &spec, headroom) == Pressure::Ok {
                            break;
                        }
                    }
                }

                // The store mutex is held only while fetching the item; release it
                // before the LLM call so other readers aren't blocked.
                let mut s = store.lock().await;
                if let Err(e) =
                    process_queue_item(&mut s, describer.as_ref(), embedder.as_ref(), &item, &cfg)
                        .await
                {
                    tracing::warn!("worker: process_queue_item error: {e}");
                }
            }
        }
    }
}
