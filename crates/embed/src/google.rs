//! Google Gemini Embeddings API adapter (text-embedding-004).
//!
//! Endpoint: POST https://generativelanguage.googleapis.com/v1beta/models/{model}:embedContent
//! Auth: `?key=GOOGLE_API_KEY` query param.
//! Override base URL via `GOOGLE_BASE_URL` env var.

use crate::Embedder;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";
pub const DEFAULT_MODEL: &str = "text-embedding-004";
/// text-embedding-004 produces 768-dimensional vectors.
pub const DEFAULT_DIM: usize = 768;

pub struct GoogleEmbedder {
    base_url: String,
    model: String,
    api_key: String,
    dim: usize,
    client: reqwest::Client,
}

impl GoogleEmbedder {
    /// Resolve Google base URL: caller-supplied > `GOOGLE_BASE_URL` env > default.
    pub fn resolve_base_url(cfg_url: Option<&str>) -> String {
        cfg_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("GOOGLE_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
    }

    /// Create using `GOOGLE_API_KEY` from the environment.
    /// Falls back to `config_key` when the env var is absent (e.g. key saved via web Settings).
    pub fn from_env(model: impl Into<String>, dim: usize) -> Result<Self> {
        Self::from_env_or_config(model, dim, None)
    }

    /// Like `from_env` but also accepts a config-file fallback key.
    pub fn from_env_or_config(
        model: impl Into<String>,
        dim: usize,
        config_key: Option<&str>,
    ) -> Result<Self> {
        let api_key = std::env::var("GOOGLE_API_KEY")
            .ok()
            .or_else(|| config_key.map(|s| s.to_string()))
            .context("GOOGLE_API_KEY not set — required for Google embeddings")?;
        Ok(Self {
            base_url: Self::resolve_base_url(None),
            model: model.into(),
            api_key,
            dim,
            client: reqwest::Client::new(),
        })
    }

    /// Create with explicit settings (for testing / custom endpoints).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        dim: usize,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            dim,
            client: reqwest::Client::new(),
        }
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct EmbedRequest {
    model: String,
    content: EmbedContent,
}

#[derive(Serialize)]
struct EmbedContent {
    parts: Vec<EmbedPart>,
}

#[derive(Serialize)]
struct EmbedPart {
    text: String,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embedding: EmbedValues,
}

#[derive(Deserialize)]
struct EmbedValues {
    values: Vec<f32>,
}

// ── Trait impl ────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl Embedder for GoogleEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model_path = format!("models/{}", self.model);
        // Use x-goog-api-key header instead of ?key= query param — keeps the key
        // out of URLs which appear in HTTP logs, error messages, and tracing output.
        let url = format!("{}/v1beta/{}:embedContent", self.base_url, model_path);

        let body = EmbedRequest {
            model: model_path,
            content: EmbedContent {
                parts: vec![EmbedPart {
                    text: text.to_string(),
                }],
            },
        };

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .context("Google embedContent request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google embedContent returned {status}: {body}");
        }

        let parsed: EmbedResponse = resp
            .json()
            .await
            .context("parsing Google embedContent response")?;

        Ok(parsed.embedding.values)
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_base_url_returns_default_when_no_env() {
        // Clear any accidental env var for the test
        std::env::remove_var("GOOGLE_BASE_URL");
        let url = GoogleEmbedder::resolve_base_url(None);
        assert_eq!(url, DEFAULT_BASE);
    }

    #[test]
    fn resolve_base_url_prefers_caller_supplied() {
        let url = GoogleEmbedder::resolve_base_url(Some("https://custom.example.com"));
        assert_eq!(url, "https://custom.example.com");
    }

    #[test]
    fn from_env_errors_without_api_key() {
        std::env::remove_var("GOOGLE_API_KEY");
        let result = GoogleEmbedder::from_env(DEFAULT_MODEL, DEFAULT_DIM);
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("GOOGLE_API_KEY"), "unexpected error: {msg}");
    }

    #[test]
    fn dim_returns_configured_value() {
        let e = GoogleEmbedder::new("https://base", DEFAULT_MODEL, "key", 768);
        assert_eq!(e.dim(), 768);
    }

    #[tokio::test]
    #[ignore = "requires GOOGLE_API_KEY env var and network access"]
    async fn live_embed_returns_correct_dim() {
        let e = GoogleEmbedder::from_env(DEFAULT_MODEL, DEFAULT_DIM).unwrap();
        let v = e.embed("hello world").await.unwrap();
        assert_eq!(v.len(), DEFAULT_DIM);
    }
}
