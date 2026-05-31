//! LLM adapter — trait + BYO-model implementations for text generation.
//!
//! Supported providers:
//! - `ollama`   — Ollama local server (`/api/generate`)
//! - `openai`   — OpenAI chat completions API (requires `OPENAI_API_KEY`)
//! - `anthropic`— Anthropic Messages API (requires `ANTHROPIC_API_KEY`)
//! - `llamacpp` — llama.cpp HTTP server (OpenAI-compatible `/v1/chat/completions`)
//! - `claude-code` — the user's Claude Pro/Max **subscription**, via the local
//!   `claude` CLI in headless print mode (no API key, no token billing)

pub mod anthropic;
pub mod claude_code;
pub mod ollama;
pub mod openai_compat;

pub use anthropic::AnthropicLlm;
pub use claude_code::{claude_status, ClaudeCodeLlm, ClaudeStatus};
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

/// HTTP status codes worth retrying — transient server errors and rate limits.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Backoff before retry `attempt` (0-based): honor `Retry-After` if present (capped at 30s),
/// otherwise exponential `0.5s · 2^attempt`, capped at 8s.
pub(crate) fn backoff_delay(
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> std::time::Duration {
    use std::time::Duration;
    if let Some(ra) = retry_after {
        return ra.min(Duration::from_secs(30));
    }
    (Duration::from_millis(500) * 2u32.saturating_pow(attempt)).min(Duration::from_secs(8))
}

/// Send a freshly-built request with bounded retries on transient failures (retryable status
/// codes + connection/timeout errors). `build` is called once per attempt because `send()`
/// consumes the builder. Used for the non-streaming cloud calls so a 429/503 during a bulk
/// summarize is retried rather than failing the item.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    max_retries: u32,
) -> reqwest::Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        match build().send().await {
            Ok(resp) if attempt < max_retries && is_retryable_status(resp.status().as_u16()) => {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);
                tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
                attempt += 1;
            }
            Ok(resp) => return Ok(resp),
            Err(e)
                if attempt < max_retries
                    && (e.is_timeout() || e.is_connect() || e.is_request()) =>
            {
                tokio::time::sleep(backoff_delay(attempt, None)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
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

    /// Best-effort: free any resident model so RAM can recover during a memory-pressure
    /// pause. The default is a no-op (cloud adapters hold no local memory); Ollama
    /// overrides this to unload its loaded model(s).
    async fn unload(&self) {}
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

    /// Best-effort: free any resident model so RAM can recover during a memory-pressure
    /// pause. The default is a no-op (cloud adapters hold no local memory); Ollama overrides
    /// it to unload its loaded model(s). Mirrors [`Generator::unload`] for callers that hold
    /// a `dyn Describer` (e.g. the CLI summary worker).
    async fn unload(&self) {}
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
    num_ctx: u32,
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
            }
            .with_num_ctx(num_ctx);
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
        "claude-code" => Ok(Box::new(ClaudeCodeLlm::single(model, None))),
        other => anyhow::bail!("unknown LLM provider: {other}"),
    }
}

/// Build a `Describer` (file + directory summaries) from config.
///
/// Unifies the previously-hardcoded `OllamaLlm::new_with_dir_model` construction
/// at the summarize/worker/web call sites so a non-Ollama provider (e.g.
/// `claude-code`, which runs summaries on the user's Claude subscription) is
/// honored everywhere. `base_url` / `num_ctx` apply to Ollama only.
pub fn describer_from_config(
    provider: &str,
    file_model: &str,
    dir_model: &str,
    base_url: &str,
    num_ctx: u32,
    claude_bin: &str,
) -> anyhow::Result<Box<dyn Describer + Send + Sync>> {
    match provider {
        "ollama" => {
            let url = OllamaLlm::resolve_base_url(if base_url.is_empty() {
                None
            } else {
                Some(base_url)
            });
            Ok(Box::new(
                OllamaLlm::new_with_dir_model(url, file_model, dir_model).with_num_ctx(num_ctx),
            ))
        }
        "claude-code" => Ok(Box::new(ClaudeCodeLlm::new(
            file_model,
            file_model,
            dir_model,
            Some(claude_bin),
        ))),
        other => anyhow::bail!(
            "provider '{other}' has no summarization (Describer) support; \
             use 'ollama' or 'claude-code'"
        ),
    }
}
