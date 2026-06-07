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

/// Force a directory's roll-up after this many consecutive defers — a panic-level
/// backstop (~5 min at the ~250 ms summarize backoff, far above the LLM request timeout
/// that normally terminalizes a hung child) so a child stranded `in_flight` can't defer
/// its parent forever. Single source of truth shared by the CLI worker, the synchronous
/// summarize loop, and the web crate's background summarize job (`indexa_query::MAX_DIR_DEFERS`).
pub const MAX_DIR_DEFERS: u32 = 1200;
