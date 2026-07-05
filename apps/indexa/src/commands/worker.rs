use anyhow::Result;
use indexa_core::{
    config::{parse_reindex_interval, Config},
    store::Store,
};
use indexa_embed::OllamaEmbedder;
use std::sync::Arc;

use super::cmd_index;
use super::helpers::{now_unix, require_index_db, select_summary_models};

/// Default re-index interval when `--auto-reindex` is passed but `[scan] auto_reindex`
/// is `off`/unset — a week is a sane "keep it fresh" cadence without being aggressive.
const DEFAULT_REINDEX_SECS: u64 = 7 * 86_400;

/// Indexed roots whose newest deep-indexed content is older than `interval_secs`.
/// Roots that have never been deep-indexed (no chunks) are skipped — auto-reindex
/// refreshes existing context, it doesn't deep-index something the user never did.
fn stale_roots(store: &Store, interval_secs: u64, now: i64) -> Result<Vec<String>> {
    let cutoff = now - interval_secs as i64;
    let mut stale = Vec::new();
    for root in store.root_paths()? {
        if let Some(ts) = store.last_indexed_at_for_root(&root)? {
            if ts < cutoff {
                stale.push(root);
            }
        }
    }
    Ok(stale)
}

/// Re-index every stale root (incremental scan→deep→summarize) before the worker
/// starts draining. Runs to completion synchronously; per-root failures only warn.
async fn run_auto_reindex(db_path: &std::path::Path, cfg: &Config) -> Result<()> {
    let interval = parse_reindex_interval(&cfg.scan.auto_reindex).unwrap_or_else(|| {
        println!(
            "auto-reindex: [scan] auto_reindex is \"{}\"; using the default 7d interval.",
            cfg.scan.auto_reindex
        );
        DEFAULT_REINDEX_SECS
    });
    let stale = {
        let store = Store::open(db_path)?;
        stale_roots(&store, interval, now_unix())?
    };
    if stale.is_empty() {
        println!("auto-reindex: all indexed roots are current (interval {interval}s). Nothing to refresh.");
        return Ok(());
    }
    println!(
        "auto-reindex: {} root(s) older than {interval}s — refreshing:",
        stale.len()
    );
    for root in &stale {
        println!("  ↻ {root}");
    }
    for root in stale {
        // Reuse the one-shot pipeline; it's incremental (deep skips unchanged files,
        // summarize refreshes stale summaries). A failure on one root must not abort
        // the others or the worker.
        if let Err(e) = cmd_index(
            vec![root.clone()],
            None,
            "augment".to_owned(),
            None,
            false,
            false, // contextual_prefix off here; config [describer] contextual_prefix still applies
            true,  // yes: worker already resolved the root; skip the huge-root guard
            cfg,
        )
        .await
        {
            eprintln!("auto-reindex: failed to refresh {root}: {e:#}");
        }
    }
    Ok(())
}

pub(crate) async fn cmd_worker(concurrency: usize, auto_reindex: bool, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    // Auto-reindex: refresh stale roots before draining (opt-in via the flag so an
    // expensive rebuild never starts implicitly).
    if auto_reindex {
        if let Err(e) = run_auto_reindex(&db_path, cfg).await {
            eprintln!("auto-reindex: skipped ({e:#})");
        }
    }

    // Pre-flight: for local Ollama, downgrade the dir roll-up model to one that fits
    // the budget (non-interactive CLI "ask me first"). For claude-code the models run
    // on the user's subscription (no local RAM to fit), so use them as configured.
    let (file_model, dir_model) = if cfg.describer.provider == "claude-code" {
        (
            cfg.describer.file_model.clone(),
            cfg.describer.dir_model.clone(),
        )
    } else {
        select_summary_models(cfg)
    };
    // Route through the factory so `provider = "claude-code"` is honored, not just Ollama.
    let describer: Arc<dyn indexa_llm::Describer + Send + Sync> =
        Arc::from(indexa_llm::describer_from_config(
            &cfg.describer.provider,
            &file_model,
            &dir_model,
            &cfg.describer.base_url,
            cfg.describer.num_ctx,
            &cfg.describer.claude_bin,
        )?);
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

    let mut summary_cfg = cfg.describer.clone();
    // Keep the cfg models truthful under auto-downgrade: summary rows record
    // cfg.file_model/dir_model as their `model`, and provenance marks the substitution.
    summary_cfg.model_fallback =
        file_model != cfg.describer.file_model || dir_model != cfg.describer.dir_model;
    summary_cfg.file_model = file_model.clone();
    summary_cfg.dir_model = dir_model.clone();
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
