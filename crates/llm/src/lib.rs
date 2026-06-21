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
pub use ollama::{caption_image_file, ollama_list_models, ollama_pull, OllamaLlm};
pub use openai_compat::OpenAICompatLlm;

// HTTP client construction + transient-failure retry policy live in the shared
// `indexa-http-util` crate (one source of truth with `indexa-embed`).
pub(crate) use indexa_http_util::{http_client, send_with_retry};

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

/// Output constraint appended to every file/dir summary prompt. The LangChain-style
/// "refine" wording ("refine the original summary…") otherwise invites a conversational
/// preamble ("Here's a refined summary:") that pollutes both the stored L1 summary and
/// the L0 abstract derived from it. Single-sourced so all providers stay consistent.
/// (A defensive [`strip_summary_preamble`](../../query/src/summarize.rs) backstop in the
/// summarize loop cleans anything that slips through.)
pub(crate) const SUMMARY_OUTPUT_RULE: &str =
    "Respond with the summary text only — no preamble, no \"Here is\"/\"Here's\", no lead-in, no closing remarks.";

/// Generates natural-language descriptions for files and directory roll-ups.
#[async_trait::async_trait]
pub trait Describer: Send + Sync {
    /// Short adapter name stamped into summary provenance ("ollama", "claude-code").
    fn provider_name(&self) -> &'static str {
        "unspecified"
    }

    /// 1–2 sentence file description from a content sample.
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
