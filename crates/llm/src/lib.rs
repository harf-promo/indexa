//! LLM description adapter — generates human-readable summaries of files.

pub mod ollama;
pub use ollama::OllamaLlm;

/// Generates a natural-language description for a file given its content sample.
#[async_trait::async_trait]
pub trait Describer: Send + Sync {
    async fn describe(&self, path: &str, content_sample: &[u8]) -> anyhow::Result<String>;
}
