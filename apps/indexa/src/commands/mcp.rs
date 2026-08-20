use anyhow::Result;
use indexa_core::config::Config;
use indexa_mcp::ToolProfile;
use std::sync::Arc;

use super::helpers::{build_embedder, build_llm, require_index_db};

/// Run the MCP (Model Context Protocol) server over stdio so AI agents
/// (Claude Desktop, Cursor, …) can browse the index live as tool calls.
/// stdout is the JSON-RPC channel — Indexa's tracing already writes to stderr only.
/// `tool_profile_flag` (3.2, `--tool-profile`) overrides `[mcp] tool_profile` in config
/// when set.
pub(crate) async fn cmd_mcp(cfg: &Config, tool_profile_flag: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(build_embedder(cfg, None)?);
    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(build_llm(cfg, None)?);
    let tool_profile = ToolProfile::parse(
        tool_profile_flag
            .as_deref()
            .unwrap_or(&cfg.mcp.tool_profile),
    );
    indexa_mcp::serve_mcp(db_path, embedder, llm, cfg.clone(), tool_profile).await
}
