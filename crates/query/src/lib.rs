//! Hybrid search (FTS5 + vector) and RAG-based Q&A pipeline.

pub mod qa;
pub use qa::{ask, synthesize_from_hits, Answer, QaConfig, SourceCitation};
