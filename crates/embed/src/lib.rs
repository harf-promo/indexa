//! Embedding adapter — trait + BYO-model implementations.

pub mod ollama;

pub use ollama::OllamaEmbedder;

/// Produces a vector embedding for a piece of text.
/// `dim()` reports the vector length so callers can allocate the right sqlite-vec column.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn dim(&self) -> usize;
}
