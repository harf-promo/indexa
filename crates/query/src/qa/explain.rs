//! Retrieval trace for `indexa ask --explain` — a read-only diagnostic that runs the
//! same retrieve + optional rerank the answer path uses and surfaces the per-retriever
//! (sparse/dense) and fused rankings, without synthesizing an answer.

use std::path::Path;

use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{AnnIndex, SearchHit, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;

use crate::rerank::{apply_rerank, CandleReranker, LlmReranker};

use super::retrieve::retrieve;
use super::QaConfig;

/// Human-readable name for a retrieval mode.
fn mode_label(m: &HybridMode) -> &'static str {
    match m {
        HybridMode::Rrf => "RRF",
        HybridMode::Sparse => "sparse",
        HybridMode::Dense => "dense",
    }
}

/// One stage of the retrieval pipeline, captured for `indexa ask --explain`.
#[derive(Debug)]
pub struct RetrievalStage {
    /// What this stage represents, e.g. "sparse (BM25)", "dense (cosine)", "fused (RRF) + weights".
    pub label: String,
    /// The hits this stage produced, in rank order (with `rrf_score` populated).
    pub hits: Vec<SearchHit>,
}

/// A retrieval trace for `indexa ask --explain`: the config used plus each pipeline
/// stage's ranked hits, so a user can see *why* the answer drew on the sources it did.
#[derive(Debug)]
pub struct RetrievalTrace {
    pub question: String,
    pub mode: String,
    pub top_k: usize,
    pub rrf_k: f32,
    pub rerank: bool,
    pub use_weights: bool,
    pub scope: Option<String>,
    pub stages: Vec<RetrievalStage>,
}

/// Build a [`RetrievalTrace`] for `indexa ask --explain` — a diagnostic view of the
/// retrieval pipeline that feeds [`answer`](super::answer). Read-only: it runs the same `retrieve`
/// (fused + boosts) and optional rerank the answer path uses, and additionally surfaces
/// the per-retriever sparse-only and dense-only rankings so a user can see how each
/// contributes. Does not synthesize an answer (the caller does that separately if wanted).
pub async fn explain_retrieval(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<RetrievalTrace> {
    // Embed once (skip for sparse-only mode), reused across the dense + fused stages.
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(question).await?),
    };

    let mut stages: Vec<RetrievalStage> = Vec::new();

    // Per-retriever breakdown + the actual fused result, in one sync store scope so the
    // `&Store` never crosses an `.await` (keeps this future `Send`).
    let fused = {
        let store = Store::open(db_path)?;

        // Sparse (BM25) alone — what keyword matching found.
        if let Ok(sparse) = store.hybrid_search_with_ann(
            question,
            None,
            &HybridMode::Sparse,
            cfg.scope.as_deref(),
            cfg.top_k,
            cfg.rrf_k,
            ann,
        ) {
            stages.push(RetrievalStage {
                label: "sparse (BM25)".to_owned(),
                hits: sparse,
            });
        }

        // Dense (cosine) alone — what semantic matching found (needs a query vector).
        if let Some(qv) = query_vec.as_deref() {
            if let Ok(dense) = store.hybrid_search_with_ann(
                question,
                Some(qv),
                &HybridMode::Dense,
                cfg.scope.as_deref(),
                cfg.top_k,
                cfg.rrf_k,
                ann,
            ) {
                stages.push(RetrievalStage {
                    label: "dense (cosine)".to_owned(),
                    hits: dense,
                });
            }
        }

        // The real fused + boosted result that feeds synthesis (exactly what `answer` uses).
        retrieve(&store, question, query_vec.as_deref(), cfg, ann)?
    };

    let mut final_label = format!("fused ({})", mode_label(&cfg.mode));
    if cfg.use_weights {
        final_label.push_str(" + weights");
    }
    // Optional rerank (async, fails open) — mirrors `retrieve_and_rerank`.
    let final_hits = if cfg.rerank && !fused.is_empty() {
        final_label.push_str(" + rerank");
        if cfg.rerank_backend == "cross-encoder" {
            apply_rerank(&CandleReranker::new(), question, fused).await
        } else {
            apply_rerank(&LlmReranker::new(llm), question, fused).await
        }
    } else {
        fused
    };
    stages.push(RetrievalStage {
        label: final_label,
        hits: final_hits,
    });

    Ok(RetrievalTrace {
        question: question.to_owned(),
        mode: mode_label(&cfg.mode).to_owned(),
        top_k: cfg.top_k,
        rrf_k: cfg.rrf_k,
        rerank: cfg.rerank,
        use_weights: cfg.use_weights,
        scope: cfg.scope.clone(),
        stages,
    })
}
