use anyhow::Result;
use indexa_core::{config::HybridMode, store::Store};
use indexa_query::{answer, answer_agentic, QaConfig};

use super::helpers::{build_embedder, build_llm, require_index_db};
use indexa_core::config::Config;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_ask(
    question: String,
    embed_model_flag: Option<String>,
    llm_model_flag: Option<String>,
    scope_flag: Option<String>,
    top_k_flag: Option<usize>,
    sparse_only: bool,
    dense_only: bool,
    agentic_flag: bool,
    max_steps_flag: Option<usize>,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };

    let store = Store::open(&db_path)?;
    let chunk_count = store.chunk_count()?;
    if chunk_count == 0 {
        println!("No deep-scanned content found. Run `indexa deep <path>` first.");
        return Ok(());
    }

    let embedder = build_embedder(cfg, embed_model_flag.as_deref())?;
    let llm = build_llm(cfg, llm_model_flag.as_deref())?;

    let mode = if sparse_only {
        HybridMode::Sparse
    } else if dense_only {
        HybridMode::Dense
    } else {
        cfg.retrieval.hybrid.clone()
    };

    let scope = scope_flag
        .as_deref()
        .map(|s| shellexpand::tilde(s).into_owned());

    // --max-steps implies --agentic; otherwise fall back to the config default.
    let agentic = agentic_flag || max_steps_flag.is_some() || cfg.retrieval.agentic;
    let max_steps = max_steps_flag.unwrap_or(cfg.retrieval.agentic_max_steps);

    let qa_cfg = QaConfig {
        top_k: top_k_flag.unwrap_or(cfg.retrieval.top_k),
        mode,
        scope,
        context_budget: cfg.retrieval.context_budget,
        rrf_k: cfg.retrieval.rrf_k as f32,
        summary_weight: cfg.retrieval.summary_weight,
        summary_depth_alpha: cfg.retrieval.summary_depth_alpha,
        rerank: cfg.retrieval.rerank,
        use_weights: cfg.retrieval.use_weights,
        max_steps,
    };

    // `store` is no longer needed by the query path — `answer` opens its own
    // scoped connection. Drop it so we don't hold two handles open.
    drop(store);

    let answer = if agentic {
        println!("Searching {chunk_count} indexed chunks (agentic, up to {max_steps} hops)...\n");
        let mut on_step = |step: usize, query: &str| {
            println!("  🔍 step {step}: {query}");
        };
        let ans = answer_agentic(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
            &mut on_step,
        )
        .await?;
        println!();
        ans
    } else {
        println!("Searching {chunk_count} indexed chunks...\n");
        answer(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
        )
        .await?
    };

    println!("Answer:\n{}\n", answer.answer);

    if !answer.sources.is_empty() {
        println!("Sources:");
        for (i, src) in answer.sources.iter().enumerate() {
            let loc = if src.heading.is_empty() {
                src.path.clone()
            } else {
                format!("{} — {}", src.path, src.heading)
            };
            println!("  [{}] {}", i + 1, loc);
        }
    }

    Ok(())
}
