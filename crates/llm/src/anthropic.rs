//! Anthropic Claude adapter — calls the Messages API.
//! API key is read from the `ANTHROPIC_API_KEY` environment variable.

use crate::Generator;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default Claude model for answer synthesis.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
/// Anthropic API base URL.
pub const API_BASE: &str = "https://api.anthropic.com";
/// Anthropic API version header value.
pub const API_VERSION: &str = "2023-06-01";

pub struct AnthropicLlm {
    model: String,
    api_key: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl AnthropicLlm {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            max_tokens: 2048,
            client: reqwest::Client::new(),
        }
    }

    /// Create using `ANTHROPIC_API_KEY` from the environment.
    /// Falls back to `config_key` when the env var is absent (e.g. key saved via web Settings).
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        Self::from_env_or_config(model, None)
    }

    /// Like `from_env` but also accepts a config-file fallback key.
    pub fn from_env_or_config(model: impl Into<String>, config_key: Option<&str>) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(|| config_key.map(|s| s.to_string()))
            .context("ANTHROPIC_API_KEY not set — required for Anthropic LLM")?;
        Ok(Self::new(model, api_key))
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[async_trait::async_trait]
impl Generator for AnthropicLlm {
    async fn generate(&self, prompt: &str) -> Result<String> {
        let url = format!("{API_BASE}/v1/messages");

        let body = MessagesRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
        };

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Anthropic Messages request to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic returned {status}: {text}");
        }

        let parsed: MessagesResponse = resp
            .json()
            .await
            .context("parsing Anthropic Messages response")?;

        let text = parsed
            .content
            .into_iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("\n");

        if text.is_empty() {
            anyhow::bail!("Anthropic response contained no text content");
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_sets_model() {
        let llm = AnthropicLlm::new("claude-opus-4-7", "sk-test");
        assert_eq!(llm.model, "claude-opus-4-7");
        assert_eq!(llm.max_tokens, 2048);
    }

    #[tokio::test]
    #[ignore = "requires ANTHROPIC_API_KEY env var and network access"]
    async fn live_generate_returns_text() {
        let llm = AnthropicLlm::from_env(DEFAULT_MODEL).unwrap();
        let answer = llm.generate("Say 'hello' in one word.").await.unwrap();
        assert!(!answer.is_empty());
    }
}
