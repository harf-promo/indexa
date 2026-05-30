use anyhow::Result;
use indexa_core::{config::Config, store::Store};
use indexa_embed::OllamaEmbedder;
use indexa_llm::OllamaLlm;
use std::sync::Arc;

use super::helpers::require_index_db;

pub(crate) async fn cmd_worker(concurrency: usize, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let base_url = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
    let describer: Arc<dyn indexa_llm::Describer + Send + Sync> =
        Arc::new(OllamaLlm::new_with_dir_model(
            &base_url,
            &cfg.describer.file_model,
            &cfg.describer.dir_model,
        ));
    let embed_base = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync> = Arc::new(OllamaEmbedder::new(
        &embed_base,
        &cfg.embedding.model,
        cfg.embedding.dim,
    ));

    let store = Arc::new(tokio::sync::Mutex::new(Store::open(&db_path)?));

    // Startup sweep before any worker claims: reset items left `in_flight` by a prior
    // crash/kill back to `pending` (failing those past the attempt cap), so they aren't
    // stranded. Must run before the worker tasks spawn.
    match store.lock().await.requeue_stale_in_flight(3) {
        Ok((requeued, failed)) if requeued > 0 || failed > 0 => println!(
            "Requeued {requeued} stale in-flight item(s) from a previous run ({failed} failed over the attempt cap)."
        ),
        Ok(_) => {}
        Err(e) => eprintln!("Warning: could not sweep stale in-flight items: {e}"),
    }

    let stats = store.lock().await.queue_stats()?;
    println!(
        "Summary worker starting ({concurrency} concurrent). Queue: {} pending, {} done, {} failed.",
        stats.pending, stats.done, stats.failed
    );
    println!("Press Ctrl-C to stop.");

    let summary_cfg = cfg.describer.clone();
    let headroom = cfg.resource.effective_headroom_bytes();
    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let s = Arc::clone(&store);
        let d = Arc::clone(&describer);
        let e = Arc::clone(&embedder);
        let c = summary_cfg.clone();
        handles.push(tokio::spawn(indexa_query::run_worker(s, d, e, c, headroom)));
    }

    // Wait for all (runs forever until Ctrl-C)
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}
