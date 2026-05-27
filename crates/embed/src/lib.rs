//! Embedding adapter — trait + BYO-model implementations.
//!
//! Supported providers:
//! - `ollama`   — Ollama local server (`/api/embeddings`), default nomic-embed-text
//! - `openai`   — OpenAI `/v1/embeddings` API (requires `OPENAI_API_KEY`)
//! - `llamacpp` — llama.cpp HTTP server (OpenAI-compatible `/v1/embeddings`)

pub mod ollama;
pub mod openai;

pub use ollama::OllamaEmbedder;
pub use openai::OpenAIEmbedder;

/// Produces a vector embedding for a piece of text.
/// `dim()` reports the vector length so callers can allocate the right buffer.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn dim(&self) -> usize;
}

/// Build an `Embedder` from config values.
/// Returns an error if credentials are missing (e.g. OPENAI_API_KEY not set).
pub fn from_config(
    provider: &str,
    model: &str,
    dim: usize,
    base_url: &str,
) -> anyhow::Result<Box<dyn Embedder>> {
    match provider {
        "ollama" => Ok(Box::new(OllamaEmbedder::new(base_url, model, dim))),
        "openai" => Ok(Box::new(OpenAIEmbedder::from_env(model, dim)?)),
        "llamacpp" => Ok(Box::new(OpenAIEmbedder::local_llamacpp(
            base_url, model, dim,
        ))),
        other => anyhow::bail!("unknown embedding provider: {other}"),
    }
}
