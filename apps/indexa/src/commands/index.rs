use anyhow::Result;
use indexa_core::config::{self, Config};
use indexa_core::store::Store;

use super::helpers::{index_db_path, preflight_ollama};
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
    contextual: bool,
    cfg: &Config,
) -> Result<()> {
    // ── Preflight: confirm Ollama is up and required models are pulled ─────────
    preflight_ollama(cfg).await?;

    // ── Phase 1: scan ─────────────────────────────────────────────────────────
    println!("── Phase 1 / 3 · Scan ──────────────────────────────────────");
    cmd_scan(paths.clone(), false, cfg).await?;

    // ── Phase 2: deep embed + code graph ──────────────────────────────────────
    println!("\n── Phase 2 / 3 · Deep context ──────────────────────────────");
    cmd_deep(
        paths.clone(),
        embed_model,
        false,
        mode.clone(),
        contextual,
        cfg,
    )
    .await?;

    // ── Phase 3: hierarchical summaries ───────────────────────────────────────
    println!("\n── Phase 3 / 3 · Summaries ─────────────────────────────────");
    cmd_summarize(paths, mode, passes, cfg).await?;

    // ── Phase 4 (quiet): decision detectors ───────────────────────────────────
    // An inbox question is a bonus, never a gate — a detector failure must not
    // fail an index build that already succeeded.
    let questions = match detector_pass(cfg) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("decision detector pass failed: {e:#}");
            0
        }
    };

    println!("\n✓ Context is ready.");
    println!("  Ask:    indexa ask \"<question>\"");
    println!("  Export: indexa export <path> --format xml > context.xml");
    if questions > 0 {
        println!("  {questions} question(s) for you — indexa review list");
    }
    Ok(())
}

/// Run the post-index detector pass; returns how many questions it opened. Also runs the
/// application/structure recognition pass (v0.66) as a sibling — it writes deterministic facts,
/// not inbox questions, so a detection failure is logged and never fails the build.
fn detector_pass(cfg: &Config) -> Result<usize> {
    let mut store = Store::open(&index_db_path()?)?;
    let report = indexa_core::decisions::detectors::run_detectors(&mut store, &cfg.review)?;

    // App/structure recognition: persist what each directory is (Rust crate, Next.js app,
    // macOS .app bundle, …). Optional user catalog lives next to the config file.
    let user_fp = config::default_config_path()
        .parent()
        .map(|d| d.join("fingerprints.json"));
    match indexa_core::fingerprint::load(user_fp.as_deref()) {
        Ok(defs) => {
            if let Err(e) = indexa_core::app_detect::detect_directory_apps(&mut store, &defs) {
                tracing::warn!("application detection failed: {e:#}");
            }
        }
        Err(e) => tracing::warn!("fingerprint library load failed: {e:#}"),
    }
    Ok(report.opened)
}
