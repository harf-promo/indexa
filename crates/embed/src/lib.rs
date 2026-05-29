//! Embedding adapter — trait + BYO-model implementations.
//!
//! Supported providers:
//! - `ollama`   — Ollama local server (`/api/embeddings`), default nomic-embed-text
//! - `openai`   — OpenAI `/v1/embeddings` API (requires `OPENAI_API_KEY`)
//! - `llamacpp` — llama.cpp HTTP server (OpenAI-compatible `/v1/embeddings`)
//! - `google`   — Google Gemini `/v1beta/models/:embedContent` (requires `GOOGLE_API_KEY`)

pub mod google;
pub mod ollama;
pub mod openai;

pub use google::GoogleEmbedder;
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
///
/// `openai_key` / `google_key` are used as fallbacks when the corresponding
/// environment variables (`OPENAI_API_KEY`, `GOOGLE_API_KEY`) are not set.
/// Pass `None` to require the env var.
pub fn from_config(
    provider: &str,
    model: &str,
    dim: usize,
    base_url: &str,
    openai_key: Option<&str>,
    google_key: Option<&str>,
) -> anyhow::Result<Box<dyn Embedder + Send + Sync>> {
    from_config_with_keep_alive(provider, model, dim, base_url, openai_key, google_key, None)
}

/// Like `from_config` but also sets `keep_alive` on Ollama adapters.
pub fn from_config_with_keep_alive(
    provider: &str,
    model: &str,
    dim: usize,
    base_url: &str,
    openai_key: Option<&str>,
    google_key: Option<&str>,
    keep_alive: Option<i64>,
) -> anyhow::Result<Box<dyn Embedder + Send + Sync>> {
    let base = if base_url.is_empty() {
        None
    } else {
        Some(base_url)
    };
    match provider {
        "ollama" => {
            let url = OllamaEmbedder::resolve_base_url(base);
            let embedder = match keep_alive {
                Some(ka) => OllamaEmbedder::new_with_keep_alive(url, model, dim, ka),
                None => OllamaEmbedder::new(url, model, dim),
            };
            Ok(Box::new(embedder))
        }
        "openai" => Ok(Box::new(OpenAIEmbedder::from_env_or_config(
            model, dim, openai_key,
        )?)),
        "llamacpp" => Ok(Box::new(OpenAIEmbedder::local_llamacpp(
            OpenAIEmbedder::resolve_base_url(base),
            model,
            dim,
        ))),
        "google" => Ok(Box::new(GoogleEmbedder::from_env_or_config(
            model, dim, google_key,
        )?)),
        other => anyhow::bail!("unknown embedding provider: {other}"),
    }
}
