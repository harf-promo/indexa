//! LLM adapter — trait + BYO-model implementations for text generation.
//!
//! Supported providers:
//! - `ollama`   — Ollama local server (`/api/generate`)
//! - `openai`   — OpenAI chat completions API (requires `OPENAI_API_KEY`)
//! - `anthropic`— Anthropic Messages API (requires `ANTHROPIC_API_KEY`)
//! - `llamacpp` — llama.cpp HTTP server (OpenAI-compatible `/v1/chat/completions`)

pub mod anthropic;
pub mod ollama;
pub mod openai_compat;

pub use anthropic::AnthropicLlm;
pub use ollama::OllamaLlm;
pub use openai_compat::OpenAICompatLlm;

/// Generates text from a prompt.
/// Implemented by all concrete LLM adapters.
#[async_trait::async_trait]
pub trait Generator: Send + Sync {
    async fn generate(&self, prompt: &str) -> anyhow::Result<String>;
}

/// Generates a natural-language description for a file given its content sample.
#[async_trait::async_trait]
pub trait Describer: Send + Sync {
    async fn describe(&self, path: &str, content_sample: &[u8]) -> anyhow::Result<String>;
}

/// Build a `Generator` from config values.
/// Returns an error if credentials are missing.
pub fn from_config(
    provider: &str,
    model: &str,
    base_url: &str,
) -> anyhow::Result<Box<dyn Generator>> {
    let base = if base_url.is_empty() {
        None
    } else {
        Some(base_url)
    };
    match provider {
        "ollama" => Ok(Box::new(OllamaLlm::new(
            OllamaLlm::resolve_base_url(base),
            model,
        ))),
        "openai" => Ok(Box::new(OpenAICompatLlm::from_env(model)?)),
        "anthropic" => Ok(Box::new(AnthropicLlm::from_env(model)?)),
        "llamacpp" => Ok(Box::new(OpenAICompatLlm::local_llamacpp(
            OpenAICompatLlm::resolve_base_url(base),
            model,
        ))),
        other => anyhow::bail!("unknown LLM provider: {other}"),
    }
}
