//! Hybrid search (FTS5 + vector) and RAG-based Q&A pipeline.

pub mod export;
pub mod qa;
pub mod summarize;
pub mod worker;

pub use export::{build_tree, render_json, render_markdown, render_xml};
pub use indexa_core::config::HybridMode;
pub use qa::{ask, synthesize_from_hits, Answer, QaConfig, SourceCitation};
pub use summarize::{enqueue_subtree, summarize_subtree_sync};
pub use worker::run_worker;
