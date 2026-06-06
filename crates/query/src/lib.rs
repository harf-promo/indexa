//! Hybrid search (FTS5 + vector) and RAG-based Q&A pipeline.

pub mod export;
pub mod qa;
pub mod rerank;
pub mod summarize;
pub mod worker;

pub use export::{build_tree, render_json, render_markdown, render_xml};
pub use indexa_core::config::HybridMode;
pub use qa::{
    answer, answer_agentic, answer_agentic_stream, answer_stream, answer_stream_with_ann,
    answer_with_ann, Answer, AnswerChunk, QaConfig, SourceCitation, AGENTIC_MAX_STEPS_CAP,
};
pub use summarize::{
    enqueue_subtree, process_queue_item_with_passes, requeue_subtree, summarize_subtree_sync,
    QueueOutcome,
};
pub use worker::run_worker;
