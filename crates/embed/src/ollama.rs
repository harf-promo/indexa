use crate::Embedder;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default Ollama embedding model — Nomic AI's nomic-embed-text (Apache-2.0, 768 dim).
/// Users override via config.toml `[embedding] model = "..."`.
pub const DEFAULT_MODEL: &str = "nomic-embed-text";
pub const DEFAULT_DIM: usize = 768;

pub struct OllamaEmbedder {
    base_url: String,
    model: String,
    dim: usize,
    client: reqwest::Client,
    /// keep_alive value to send with every request (seconds).
    /// 0 = unload immediately; -1 = keep forever; None = server default.
    keep_alive: Option<i64>,
}

const DEFAULT_BASE: &str = "http://localhost:11434";

impl OllamaEmbedder {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dim,
            client: reqwest::Client::new(),
            keep_alive: None,
        }
    }

    /// Construct with an explicit keep_alive (seconds).
    /// Use `0` to unload the model immediately after each call.
    pub fn new_with_keep_alive(
        base_url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
        keep_alive: i64,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dim,
            client: reqwest::Client::new(),
            keep_alive: Some(keep_alive),
        }
    }

    /// Resolve the Ollama base URL: caller-supplied > `OLLAMA_HOST` env > default.
    pub fn resolve_base_url(cfg_url: Option<&str>) -> String {
        cfg_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OLLAMA_HOST").ok())
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
    }

    pub fn default_local() -> Self {
        Self::new(Self::resolve_base_url(None), DEFAULT_MODEL, DEFAULT_DIM)
    }

    /// Explicitly unload a model from Ollama by sending keep_alive=0.
    /// This is a best-effort call — errors are logged but not propagated.
    pub async fn unload(&self, model: &str) {
        let url = format!("{}/api/embeddings", self.base_url);
        // Ollama interprets keep_alive=0 as "unload now".
        let body = serde_json::json!({
            "model": model,
            "prompt": "",
            "keep_alive": 0
        });
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            tracing::warn!("Failed to unload model '{model}' from Ollama: {e}");
        }
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    /// Seconds to keep the model loaded after this call.
    /// 0 = unload immediately.  Omitted when None (server default applies).
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<i64>,
    /// Inference options forwarded to Ollama (num_parallel, num_ctx, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<EmbedOptions>,
}

#[derive(Serialize)]
struct EmbedOptions {
    /// Lock to 1 parallel slot to prevent KV-cache multiplication.
    num_parallel: u32,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

#[async_trait::async_trait]
impl Embedder for OllamaEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/api/embeddings", self.base_url);
        let body = EmbedRequest {
            model: &self.model,
            prompt: text,
            keep_alive: self.keep_alive,
            options: Some(EmbedOptions { num_parallel: 1 }),
        };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Ollama request to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {text}");
        }

        let parsed: EmbedResponse = resp
            .json()
            .await
            .context("parsing Ollama embedding response")?;
        Ok(parsed.embedding)
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires running Ollama with nomic-embed-text pulled"]
    async fn live_embed_returns_768_dims() {
        let embedder = OllamaEmbedder::default_local();
        let v = embedder.embed("hello world").await.unwrap();
        assert_eq!(v.len(), DEFAULT_DIM);
    }
}
