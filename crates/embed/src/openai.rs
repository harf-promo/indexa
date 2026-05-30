//! OpenAI Embeddings API adapter.
//! Also compatible with any OpenAI-API-compatible server (llama.cpp, LM Studio, etc.)
//! by setting a custom `base_url`.

use crate::Embedder;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default OpenAI embedding model and its output dimension.
pub const DEFAULT_MODEL: &str = "text-embedding-3-small";
pub const DEFAULT_DIM: usize = 1536;

/// Embedding adapter for the OpenAI `/v1/embeddings` API.
/// Works with:
/// - OpenAI (`https://api.openai.com`)
/// - llama.cpp server (`http://localhost:8080`)
/// - LM Studio, Ollama OpenAI-compat, etc.
pub struct OpenAIEmbedder {
    base_url: String,
    model: String,
    dim: usize,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAIEmbedder {
    /// Create with explicit settings.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dim,
            api_key: api_key.into(),
            client: crate::http_client(30),
        }
    }

    /// Resolve OpenAI base URL: caller-supplied > `OPENAI_BASE_URL` env > default.
    pub fn resolve_base_url(cfg_url: Option<&str>) -> String {
        cfg_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com".to_string())
    }

    /// Create using `OPENAI_API_KEY` from the environment.
    /// Falls back to `config_key` when the env var is absent (e.g. key saved via web Settings).
    pub fn from_env(model: impl Into<String>, dim: usize) -> Result<Self> {
        Self::from_env_or_config(model, dim, None, None)
    }

    /// Like `from_env` but also accepts a config-file fallback key and an optional base URL
    /// (so a configured OpenAI-compatible gateway is honoured, not silently dropped).
    pub fn from_env_or_config(
        model: impl Into<String>,
        dim: usize,
        config_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .or_else(|| config_key.map(|s| s.to_string()))
            .context("OPENAI_API_KEY not set — required for OpenAI embeddings")?;
        let base_url = Self::resolve_base_url(base_url);
        Ok(Self::new(base_url, model, dim, api_key))
    }

    /// Create for a local llama.cpp server (no API key needed).
    pub fn local_llamacpp(
        base_url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
    ) -> Self {
        Self::new(base_url, model, dim, "")
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[derive(Deserialize)]
struct EmbedItem {
    embedding: Vec<f32>,
}

#[async_trait::async_trait]
impl Embedder for OpenAIEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbedRequest {
            model: &self.model,
            input: text,
        };

        let mut builder = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }

        let resp = builder
            .send()
            .await
            .with_context(|| format!("OpenAI embeddings request to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI embeddings returned {status}: {body}");
        }

        let parsed: EmbedResponse = resp
            .json()
            .await
            .context("parsing OpenAI embeddings response")?;

        parsed
            .data
            .into_iter()
            .next()
            .map(|item| item.embedding)
            .context("OpenAI embeddings response contained no items")
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_sets_fields() {
        let e = OpenAIEmbedder::new(
            "https://api.openai.com",
            "text-embedding-3-small",
            1536,
            "sk-test",
        );
        assert_eq!(e.dim(), 1536);
    }

    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY env var and network access"]
    async fn live_embed_returns_correct_dim() {
        let e = OpenAIEmbedder::from_env(DEFAULT_MODEL, DEFAULT_DIM).unwrap();
        let v = e.embed("hello world").await.unwrap();
        assert_eq!(v.len(), DEFAULT_DIM);
    }
}
