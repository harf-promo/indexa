//! RAG-based Q&A pipeline: embed → retrieve → (rerank) → synthesize a cited answer.
//!
//! The pipeline is split across focused submodules; this file holds the shared types
//! ([`Answer`], [`SourceCitation`], [`AnswerChunk`], [`QaConfig`]) and re-exports the
//! public surface so the `qa::` path stays stable (`lib.rs` / `eval.rs` import from here):
//!
//! - `confidence` — retrieval-shape confidence ([`assess_confidence`]).
//! - `mmr` — MMR diversity re-ranking.
//! - `retrieve` — hybrid search + score boosts, archive/code-intent adjustment,
//!   broad-question detection, the project-overview composer ([`build_project_overview`]).
//! - `explain` — the `ask --explain` retrieval trace ([`explain_retrieval`]).
//! - `synthesize` — the public entry points ([`answer`], [`answer_stream`], …).
//! - `agentic` — the opt-in multi-step self-ask loop ([`answer_agentic`]).
//!
//! See `docs/architecture.md` for how this crate fits the wider system.

use indexa_core::config::HybridMode;

mod agentic;
mod cluster;
mod confidence;
mod explain;
mod mmr;
mod retrieve;
mod rewrite;
mod synthesize;

#[cfg(test)]
mod tests;

pub use agentic::{
    answer_agentic, answer_agentic_history, answer_agentic_stream, answer_agentic_stream_history,
    AGENTIC_MAX_STEPS_CAP,
};
pub use confidence::{assess_confidence, Confidence, ConfidenceInputs, ConfidenceReport};
pub use explain::{explain_retrieval, RetrievalStage, RetrievalTrace};
pub(crate) use retrieve::retrieve;
pub use retrieve::{build_project_overview, is_broad_intent};
pub use synthesize::{
    answer, answer_catalog, answer_catalog_history, answer_retrieval_only,
    answer_retrieval_only_history, answer_stream, answer_stream_with_ann,
    answer_stream_with_ann_history, answer_with_ann, answer_with_ann_history,
};
// Shared prompt-injection scrub, reused by the reranker prompt (in the sibling `rerank` module).
pub(crate) use synthesize::neutralize_fence;

/// A prior conversation turn, folded into the prompt + used to rewrite a follow-up into
/// a standalone query. Chronological order (oldest first). Deliberately decoupled from
/// the store's `ConversationTurn` (surfaces map one to the other) so the qa crate stays
/// schema-agnostic.
#[derive(Debug, Clone)]
pub struct PriorTurn {
    pub question: String,
    pub answer: String,
}

/// Result of a Q&A query.
#[derive(Debug)]
pub struct Answer {
    pub question: String,
    /// The answer text when `synthesized`, or the **retrieved context slice** (the exact
    /// pack the LLM would have seen) when `!synthesized` — see [`answer_retrieval_only`].
    pub answer: String,
    pub sources: Vec<SourceCitation>,
    /// Retrieval-shape confidence (see [`assess_confidence`]). `None` only for the
    /// zero-hit short-circuit — that message already says the index has nothing,
    /// so bolting a confidence label onto it would be noise.
    pub confidence: Option<ConfidenceReport>,
    /// `true` (default) when `answer` was synthesized by the local LLM; `false` when this is
    /// the raw retrieved slice from [`answer_retrieval_only`] (the caller synthesizes itself).
    pub synthesized: bool,
    /// Identifier of the model that synthesized `answer` (e.g. `"ollama/gemma3:12b"`), so a
    /// caller knows it was Indexa's LOCAL model — not their own. `None` when not synthesized
    /// or when the surface did not stamp it. The qa pipeline leaves this `None`; each surface
    /// (CLI/web/MCP) stamps it from the effective `[describer]` config it already holds.
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceCitation {
    pub path: String,
    pub heading: String,
    pub snippet: String,
}

/// An event emitted by [`answer_stream`]: the cited sources once up front (so a UI can
/// render citations before any token arrives), then answer text fragments as the model
/// produces them. Providers without real token streaming (everything but Ollama today)
/// emit a single `Fragment` with the whole answer.
pub enum AnswerChunk {
    Sources(Vec<SourceCitation>),
    Fragment(String),
    /// Agentic progress: hop number (1-based) + the query being searched this hop.
    /// Emitted only by [`answer_agentic_stream`]; one-shot streams never produce it.
    Step(usize, String),
}

/// Configuration for the Q&A pipeline.
#[derive(Clone)]
pub struct QaConfig {
    pub top_k: usize,
    /// Max characters of context to include in the LLM prompt.
    pub context_budget: usize,
    /// Retrieval mode (RRF / sparse / dense).
    pub mode: HybridMode,
    /// Limit search to paths starting with this prefix (tilde-expanded).
    pub scope: Option<String>,
    /// RRF rank constant (industry default: 60).
    pub rrf_k: f32,
    /// Weight applied to parent-directory summary similarity boost (0.0 = disabled).
    pub summary_weight: f32,
    /// Depth-boost coefficient α for summary cosine search.
    pub summary_depth_alpha: f32,
    /// Apply a rerank pass after retrieval (default on; `"llm"` backend reuses the
    /// loaded generation model). Fails open.
    pub rerank: bool,
    /// Which reranker backend to use when `rerank = true`.
    /// `"llm"` = listwise LLM call (default). `"cross-encoder"` = candle DeBERTa-v2.
    pub rerank_backend: String,
    /// HuggingFace model id for the `"cross-encoder"` backend (from `[retrieval] rerank_model`).
    /// Default `mixedbread-ai/mxbai-rerank-xsmall-v1`; base/large-v1 are same-arch drop-ins.
    pub rerank_model: String,
    /// Apply importance weights (v0.8) as a multiplicative boost after RRF fusion.
    pub use_weights: bool,
    /// Apply a recency boost (v0.31) — multiplies up recently-modified files (the positive twin
    /// of the archive penalty). Opt-in; off by default so it never silently re-ranks answers.
    pub use_recency_weight: bool,
    /// Recency window in days for `use_recency_weight` (files older than this stay neutral).
    pub recency_days: i64,
    /// Max retrieval hops for the agentic ([`answer_agentic`]) path. Clamped to
    /// `1..=AGENTIC_MAX_STEPS_CAP`. Ignored by the one-shot [`answer`].
    pub max_steps: usize,
    /// MMR (Maximal Marginal Relevance) lambda (v0.42).
    /// `1.0` disables MMR (pure relevance). `0.5` = balanced (default).
    /// `0.0` = maximum diversity. Mirrors the `[retrieval] mmr_lambda` config field.
    pub mmr_lambda: f32,
    /// Path segments that mark content as historical/superseded (v0.29). Hits under
    /// such a path are down-weighted by `archive_penalty`. Empty = penalty disabled.
    /// Mirrors the `[retrieval] archive_segments` config field.
    pub archive_segments: Vec<String>,
    /// Multiplicative archive down-weighting factor (v0.29). `0.0` disables the penalty.
    /// Mirrors the `[retrieval] archive_penalty` config field.
    pub archive_penalty: f64,
    /// GraphRAG-lite (v0.69): on a **broad, unscoped** question, cap how many chunks one file may
    /// contribute to the retrieved pool before other files get their turn, so one chunk-dense
    /// file can't monopolise the packed context. `0` (default) disables it entirely. Only applied
    /// when `is_broad_intent(question)` AND `scope.is_none()` — focused and scoped asks are
    /// untouched. Mirrors the `[retrieval] broad_per_file_cap` config field.
    pub broad_per_file_cap: usize,
    /// GraphRAG "Approach C" (v0.70): on a **broad, unscoped** question, group the retrieved hits
    /// into semantic clusters and present topic-grouped context to the synthesizer. `false`
    /// (default) ⇒ today's flat packing, byte-identical. Only applied when `is_broad_intent` AND
    /// `scope.is_none()`. Mirrors `[retrieval] graphrag_clusters`.
    pub graphrag_clusters: bool,
    /// Max clusters when `graphrag_clusters` is on (also caps the per-cluster summarization calls).
    /// Mirrors `[retrieval] graphrag_max_clusters`.
    pub graphrag_max_clusters: usize,
    /// Cosine-similarity threshold for joining a hit to an existing cluster (`graphrag_clusters`).
    /// Mirrors `[retrieval] graphrag_cluster_sim`.
    pub graphrag_cluster_sim: f32,
    /// When `graphrag_clusters` is on, also summarize each multi-member cluster into a one-line
    /// theme with one extra local LLM call (≤ `graphrag_max_clusters` calls; fail-open). Separate
    /// sub-flag so clustering can be used without the added latency. Mirrors
    /// `[retrieval] graphrag_summarize`.
    pub graphrag_summarize: bool,
}

impl Default for QaConfig {
    fn default() -> Self {
        Self {
            top_k: 12,
            context_budget: 8000,
            mode: HybridMode::Rrf,
            scope: None,
            rrf_k: 60.0,
            summary_weight: 0.0,
            summary_depth_alpha: 0.15,
            rerank: true,
            rerank_backend: "llm".to_string(),
            rerank_model: "mixedbread-ai/mxbai-rerank-xsmall-v1".to_string(),
            use_weights: true,
            use_recency_weight: false,
            recency_days: 90,
            max_steps: 3,
            mmr_lambda: 0.5,
            archive_segments: indexa_core::config::default_archive_segments(),
            archive_penalty: indexa_core::config::DEFAULT_ARCHIVE_PENALTY,
            broad_per_file_cap: 0,
            graphrag_clusters: false,
            graphrag_max_clusters: 4,
            graphrag_cluster_sim: 0.55,
            graphrag_summarize: false,
        }
    }
}

impl QaConfig {
    /// Build a `QaConfig` from `[retrieval]` config — the single place that maps every field the
    /// config owns (with its name remaps: `mode`←`hybrid`, `use_recency_weight`←`recency_boost`,
    /// `max_steps`←`agentic_max_steps`). A new `[retrieval]` knob is threaded to ask/search/explain
    /// in ONE spot, so those surfaces can't silently diverge (e.g. MCP `ask` vs `explain_retrieval`).
    /// `scope` starts `None`; callers apply per-request overrides (top_k, mode, rerank, scope, …).
    pub fn from_retrieval(r: &indexa_core::config::RetrievalConfig) -> Self {
        Self {
            top_k: r.top_k,
            context_budget: r.context_budget,
            mode: r.hybrid.clone(),
            scope: None,
            rrf_k: r.rrf_k as f32,
            summary_weight: r.summary_weight,
            summary_depth_alpha: r.summary_depth_alpha,
            rerank: r.rerank,
            rerank_backend: r.rerank_backend.clone(),
            rerank_model: r.rerank_model.clone(),
            use_weights: r.use_weights,
            use_recency_weight: r.recency_boost,
            recency_days: r.recency_days,
            max_steps: r.agentic_max_steps,
            mmr_lambda: r.mmr_lambda,
            archive_segments: r.archive_segments.clone(),
            archive_penalty: r.archive_penalty,
            broad_per_file_cap: r.broad_per_file_cap,
            graphrag_clusters: r.graphrag_clusters,
            graphrag_max_clusters: r.graphrag_max_clusters,
            graphrag_cluster_sim: r.graphrag_cluster_sim,
            graphrag_summarize: r.graphrag_summarize,
        }
    }
}
