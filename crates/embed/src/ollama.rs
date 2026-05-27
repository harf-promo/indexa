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
}

impl OllamaEmbedder {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dim,
            client: reqwest::Client::new(),
        }
    }

    pub fn default_local() -> Self {
        Self::new("http://localhost:11434", DEFAULT_MODEL, DEFAULT_DIM)
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
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
