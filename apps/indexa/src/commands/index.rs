use anyhow::Result;
use indexa_core::config::Config;

use super::{cmd_deep, cmd_scan, cmd_summarize};

/// One-shot context build: scan → deep embed → summarize.
///
/// Equivalent to running `indexa scan`, `indexa deep`, and `indexa summarize`
/// in sequence, but in a single command — ideal for first-time setup or full
/// refreshes. Each phase prints its own progress.
pub(crate) async fn cmd_index(
    paths: Vec<String>,
    embed_model: Option<String>,
    mode: String,
    passes: Option<u32>,
    cfg: &Config,
) -> Result<()> {
    // ── Phase 1: scan ─────────────────────────────────────────────────────────
    println!("── Phase 1 / 3 · Scan ──────────────────────────────────────");
    cmd_scan(paths.clone(), false).await?;

    // ── Phase 2: deep embed + code graph ──────────────────────────────────────
    println!("\n── Phase 2 / 3 · Deep context ──────────────────────────────");
    cmd_deep(paths.clone(), embed_model, false, mode.clone(), cfg).await?;

    // ── Phase 3: hierarchical summaries ───────────────────────────────────────
    println!("\n── Phase 3 / 3 · Summaries ─────────────────────────────────");
    cmd_summarize(paths, mode, passes, cfg).await?;

    println!("\n✓ Context is ready.");
    println!("  Ask:    indexa ask \"<question>\"");
    println!("  Export: indexa export <path> --format xml > context.xml");
    Ok(())
}
