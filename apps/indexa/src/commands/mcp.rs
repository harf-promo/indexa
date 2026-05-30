use anyhow::Result;
use indexa_core::config::Config;
use std::sync::Arc;

use super::helpers::{build_embedder, build_llm, require_index_db};

/// Run the MCP (Model Context Protocol) server over stdio so AI agents
/// (Claude Desktop, Cursor, …) can browse the index live as tool calls.
/// stdout is the JSON-RPC channel — Indexa's tracing already writes to stderr only.
pub(crate) async fn cmd_mcp(cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(build_embedder(cfg, None)?);
    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(build_llm(cfg, None)?);
    indexa_mcp::serve_mcp(db_path, embedder, llm, cfg.clone()).await
}
