use anyhow::Result;
use indexa_core::config::Config;
use std::sync::Arc;

use super::helpers::{build_embedder, build_llm, require_index_db};

pub(crate) async fn cmd_serve(
    port: u16,
    embed_model_flag: Option<String>,
    llm_model_flag: Option<String>,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = indexa_core::store::Store::open(&db_path)?;

    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(build_embedder(cfg, embed_model_flag.as_deref())?);
    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(build_llm(cfg, llm_model_flag.as_deref())?);

    indexa_web::serve(port, store, embedder, llm, cfg.clone()).await
}
