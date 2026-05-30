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

/// Build a reqwest client with a finite request + connect timeout, shared by every LLM
/// adapter. Without a timeout a stalled cloud endpoint hangs `generate()` indefinitely, and
/// these run inside the indexing worker and web/MCP request paths. `expect` is appropriate:
/// `build()` only fails on unrecoverable rustls TLS init, and never silently yields a
/// no-timeout client (unlike the old `.unwrap_or_default()`).
pub(crate) fn http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("building reqwest client (rustls TLS init)")
}

/// Generates text from a prompt.
/// Implemented by all concrete LLM adapters.
#[async_trait::async_trait]
pub trait Generator: Send + Sync {
    async fn generate(&self, prompt: &str) -> anyhow::Result<String>;

    /// Streaming variant — calls `on_fragment` with each token/chunk as it arrives.
    /// Returns the complete concatenated response.
    /// The default impl buffers the full response and calls `on_fragment` once.
    async fn generate_stream(
        &self,
        prompt: &str,
        on_fragment: &mut (dyn FnMut(String) + Send),
    ) -> anyhow::Result<String> {
        let full = self.generate(prompt).await?;
        on_fragment(full.clone());
        Ok(full)
    }
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

    /// Streaming variant of `describe`: calls `on_fragment` with each token as it arrives.
    /// The default implementation buffers the full result and calls `on_fragment` once.
    /// Providers that support token streaming (e.g. Ollama) override this.
    async fn describe_stream(
        &self,
        path: &str,
        content_sample: &[u8],
        previous_summary: Option<&str>,
        on_fragment: &mut (dyn FnMut(String) + Send),
    ) -> anyhow::Result<String> {
        let full = self
            .describe(path, content_sample, previous_summary)
            .await?;
        on_fragment(full.clone());
        Ok(full)
    }

    /// Streaming variant of `summarize_dir`: calls `on_fragment` with each token.
    /// The default implementation buffers the full result and calls `on_fragment` once.
    async fn summarize_dir_stream(
        &self,
        dir_path: &str,
        children: &[ChildSummary],
        previous_summary: Option<&str>,
        on_fragment: &mut (dyn FnMut(String) + Send),
    ) -> anyhow::Result<String> {
        let full = self
            .summarize_dir(dir_path, children, previous_summary)
            .await?;
        on_fragment(full.clone());
        Ok(full)
    }
}

/// Build a `Generator` from config values, optionally setting `keep_alive` on Ollama adapters.
///
/// `openai_key` / `anthropic_key` are used as fallbacks when the corresponding
/// environment variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) are not set.
/// Pass `None` to require the env var.
pub fn from_config_with_keep_alive(
    provider: &str,
    model: &str,
    base_url: &str,
    openai_key: Option<&str>,
    anthropic_key: Option<&str>,
    keep_alive: Option<i64>,
) -> anyhow::Result<Box<dyn Generator + Send + Sync>> {
    let base = if base_url.is_empty() {
        None
    } else {
        Some(base_url)
    };
    match provider {
        "ollama" => {
            let url = OllamaLlm::resolve_base_url(base);
            let llm = match keep_alive {
                Some(ka) => OllamaLlm::new_with_keep_alive(url, model, None, ka),
                None => OllamaLlm::new(url, model),
            };
            Ok(Box::new(llm))
        }
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
