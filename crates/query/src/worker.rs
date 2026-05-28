//! Background summarization worker — drains the summary_queue table.

use crate::summarize::process_queue_item;
use indexa_core::{config::DescriberConfig, store::Store};
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
