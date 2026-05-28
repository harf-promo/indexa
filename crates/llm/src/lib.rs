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

/// A child entry fed into a directory roll-up summary prompt.
pub struct ChildSummary {
    pub name: String,
    pub kind: String, // "file" | "dir"
    pub summary: String,
}

/// Generates natural-language descriptions for files and directory roll-ups.
#[async_trait::async_trait]
pub trait Describer: Send + Sync {
    /// One-sentence file description from a content sample.
    /// `previous_summary` is the prior draft on a refinement pass (None on first pass).
    async fn describe(
        &self,
        path: &str,
        content_sample: &[u8],
        previous_summary: Option<&str>,
    ) -> anyhow::Result<String>;

    /// 2–4 sentence directory summary synthesised from direct children.
    /// `previous_summary` is the prior draft on a refinement pass (None on first pass).
    async fn summarize_dir(
        &self,
        dir_path: &str,
        children: &[ChildSummary],
        previous_summary: Option<&str>,
    ) -> anyhow::Result<String>;
}

/// Build a `Generator` from config values.
///
/// `openai_key` / `anthropic_key` are used as fallbacks when the corresponding
/// environment variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) are not set.
/// Pass `None` to require the env var.
pub fn from_config(
    provider: &str,
    model: &str,
    base_url: &str,
    openai_key: Option<&str>,
    anthropic_key: Option<&str>,
) -> anyhow::Result<Box<dyn Generator + Send + Sync>> {
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
        "openai" => Ok(Box::new(OpenAICompatLlm::from_env_or_config(
            model, openai_key,
        )?)),
        "anthropic" => Ok(Box::new(AnthropicLlm::from_env_or_config(
            model,
            anthropic_key,
        )?)),
        "llamacpp" => Ok(Box::new(OpenAICompatLlm::local_llamacpp(
            OpenAICompatLlm::resolve_base_url(base),
            model,
        ))),
        other => anyhow::bail!("unknown LLM provider: {other}"),
    }
}
