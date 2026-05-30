use anyhow::Result;
use indexa_core::{config::Config, store::Store};
use indexa_embed::OllamaEmbedder;
use indexa_llm::OllamaLlm;
use indexa_query::summarize_subtree_sync;

use super::helpers::{parse_summary_mode, require_index_db, resolve_roots};

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

    let base_url = OllamaLlm::resolve_base_url(Some(&cfg.describer.base_url));
    let describer = OllamaLlm::new_with_dir_model(
        &base_url,
        &cfg.describer.file_model,
        &cfg.describer.dir_model,
    );
    let embed_base = OllamaEmbedder::resolve_base_url(Some(&cfg.embedding.base_url));
    let embedder = OllamaEmbedder::new(&embed_base, &cfg.embedding.model, cfg.embedding.dim);

    let mut store = Store::open(&db_path)?;

    for root in &roots {
        println!("Summarizing {} …", root.display());
        let done = summarize_subtree_sync(
            &mut store,
            &describer,
            &embedder,
            root,
            &summary_cfg,
            passes,
        )
        .await?;
        println!("  {done} summaries written.");
    }

    Ok(())
}
