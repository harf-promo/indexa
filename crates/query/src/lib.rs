//! Hybrid search (FTS5 + vector) and RAG-based Q&A pipeline.

pub mod qa;
pub mod summarize;
pub mod worker;

pub use indexa_core::config::HybridMode;
pub use qa::{ask, synthesize_from_hits, Answer, QaConfig, SourceCitation};
pub use summarize::{enqueue_subtree, summarize_subtree_sync};
pub use worker::run_worker;
