//! OpenAI-compatible chat completions adapter.
//! Works with: OpenAI, llama.cpp (`--server` mode), LM Studio, Ollama OpenAI-compat endpoint, etc.
//! Set `base_url = "http://localhost:8080"` for llama.cpp.

use crate::Generator;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default OpenAI model for answer synthesis.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const OPENAI_BASE: &str = "https://api.openai.com";

pub struct OpenAICompatLlm {
    base_url: String,
    model: String,
    api_key: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl OpenAICompatLlm {
    /// Create with explicit settings.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            max_tokens: 2048,
            client: reqwest::Client::new(),
        }
    }

    /// Create using `OPENAI_API_KEY` for the OpenAI cloud API.
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY not set — required for OpenAI LLM")?;
        Ok(Self::new(OPENAI_BASE, model, api_key))
    }

    /// Create for a local llama.cpp server (no API key needed).
    pub fn local_llamacpp(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new(base_url, model, "")
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessageResp,
}

#[derive(Deserialize)]
struct ChatMessageResp {
    content: String,
}

#[async_trait::async_trait]
impl Generator for OpenAICompatLlm {
    async fn generate(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let body = ChatRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
        };

        let mut builder = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }

        let resp = builder
            .send()
            .await
            .with_context(|| format!("OpenAI-compat request to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI-compat returned {status}: {text}");
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .context("parsing OpenAI-compat chat response")?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .context("OpenAI-compat response had no choices")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llamacpp_constructor_has_empty_key() {
        let llm = OpenAICompatLlm::local_llamacpp("http://localhost:8080", "llama3.2");
        assert_eq!(llm.api_key, "");
        assert_eq!(llm.base_url, "http://localhost:8080");
    }

    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY env var and network access"]
    async fn live_openai_generate() {
        let llm = OpenAICompatLlm::from_env(DEFAULT_MODEL).unwrap();
        let answer = llm.generate("Say 'hello' in one word.").await.unwrap();
        assert!(!answer.is_empty());
    }
}
