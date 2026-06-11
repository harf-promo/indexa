use anyhow::Result;
use indexa_core::{config::Config, store::Store};
use indexa_embed::OllamaEmbedder;
use indexa_query::summarize_subtree_sync;

use super::helpers::{parse_summary_mode, require_index_db, resolve_roots, select_summary_models};

pub(crate) async fn cmd_summarize(
    paths: Vec<String>,
    mode: String,
    passes: Option<u32>,
    cfg: &Config,
) -> Result<()> {
    let roots = resolve_roots(paths, false)?;
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let mut summary_cfg = cfg.describer.clone();
    summary_cfg.mode = parse_summary_mode(&mode)?;

    // Pre-flight: for local Ollama, downgrade the dir roll-up model to one that
    // fits the budget (non-interactive CLI "ask me first"). For claude-code the
    // models run on the user's subscription (no local RAM to fit), so use them as-is.
    let (file_model, dir_model) = if cfg.describer.provider == "claude-code" {
        (
            cfg.describer.file_model.clone(),
            cfg.describer.dir_model.clone(),
        )
    } else {
        select_summary_models(cfg)
    };
    // Keep the cfg models truthful: the summary rows record cfg.file_model/dir_model
    // as their `model`, so a silent downgrade must be reflected there too (otherwise
    // provenance records a model that never ran). model_fallback marks the substitution.
    summary_cfg.model_fallback =
        file_model != cfg.describer.file_model || dir_model != cfg.describer.dir_model;
    summary_cfg.file_model = file_model.clone();
    summary_cfg.dir_model = dir_model.clone();
    let describer = indexa_llm::describer_from_config(
        &cfg.describer.provider,
        &file_model,
        &dir_model,
        &cfg.describer.base_url,
        cfg.describer.num_ctx,
        &cfg.describer.claude_bin,
    )?;
    let embed_base = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let embedder = OllamaEmbedder::new(&embed_base, &cfg.embedding.model, cfg.embedding.dim);

    let mut store = Store::open(&db_path)?;

    for root in &roots {
        println!("Summarizing {} …", root.display());
        let (done, skipped) = summarize_subtree_sync(
            &mut store,
            describer.as_ref(),
            &embedder,
            root,
            &summary_cfg,
            passes,
        )
        .await?;
        if skipped > 0 {
            // A near-instant refresh must explain itself, or it looks broken.
            println!("  {done} summaries written, {skipped} unchanged (skipped).");
        } else {
            println!("  {done} summaries written.");
        }
    }

    Ok(())
}
