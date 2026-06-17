//! Hybrid search (FTS5 + vector) and RAG-based Q&A pipeline.

pub mod contextual;
pub mod eval;
pub mod export;
pub mod impact;
pub mod qa;
pub mod redact;
pub mod rerank;
pub mod summarize;
pub mod worker;

pub use eval::{
    aggregate, evaluate_question, score_ranking, EvalQuestion, EvalSummary, GoldenSet,
    QuestionMetrics,
};
pub use export::{
    approx_tokens, build_tree, prune_tree, render_graph, render_json, render_markdown,
    render_signatures, render_weights, render_xml,
};
pub use impact::{served_bytes, AnswerImpact};
pub use indexa_core::config::HybridMode;
pub use qa::{
    answer, answer_agentic, answer_agentic_stream, answer_stream, answer_stream_with_ann,
    answer_with_ann, assess_confidence, build_project_overview, explain_retrieval, is_broad_intent,
    Answer, AnswerChunk, Confidence, ConfidenceInputs, ConfidenceReport, QaConfig, RetrievalStage,
    RetrievalTrace, SourceCitation, AGENTIC_MAX_STEPS_CAP,
};
pub use summarize::{
    enqueue_subtree, process_queue_item_with_passes, requeue_subtree, summarize_subtree_sync,
    QueueOutcome,
};
pub use worker::run_worker;

/// Force a directory's roll-up after this many consecutive defers — a panic-level
/// backstop (~5 min at the ~250 ms summarize backoff, far above the LLM request timeout
/// that normally terminalizes a hung child) so a child stranded `in_flight` can't defer
/// its parent forever. Single source of truth shared by the CLI worker, the synchronous
/// summarize loop, and the web crate's background summarize job (`indexa_query::MAX_DIR_DEFERS`).
pub const MAX_DIR_DEFERS: u32 = 1200;
