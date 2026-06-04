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
    // Enable the web "Update now" button for CLI serve users.
    // The update replaces the binary on disk; the user must restart `indexa serve` manually
    // (unlike the desktop app which calls app.restart()). Gated on this var so the endpoint
    // stays disabled in headless/library contexts that embed indexa_web::serve directly.
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("INDEXA_WEB_ALLOW_UPDATE", "1");
    }

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
