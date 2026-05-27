use crate::{Describer, Generator};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default generation model — users override via config.toml `[describer] model = "..."`.
pub const DEFAULT_MODEL: &str = "qwen2.5:14b";

pub struct OllamaLlm {
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) client: reqwest::Client,
}

impl OllamaLlm {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    pub fn default_local() -> Self {
        Self::new("http://localhost:11434", DEFAULT_MODEL)
    }
}

#[derive(Serialize)]
struct Req<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct Resp {
    response: String,
}

#[async_trait::async_trait]
impl Generator for OllamaLlm {
    /// Send a prompt to Ollama's `/api/generate` and return the response text.
    async fn generate(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);
        let body = Req {
            model: &self.model,
            prompt,
            stream: false,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Ollama generate request to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {text}");
        }

        let parsed: Resp = resp
            .json()
            .await
            .context("parsing Ollama generate response")?;
        Ok(parsed.response)
    }
}

#[async_trait::async_trait]
impl Describer for OllamaLlm {
    async fn describe(&self, path: &str, content_sample: &[u8]) -> Result<String> {
        let sample = std::str::from_utf8(content_sample)
            .unwrap_or("[binary]")
            .chars()
            .take(500)
            .collect::<String>();
        let prompt = format!(
            "Briefly describe what this file is about in 1-2 sentences.\nFile: {path}\nContent:\n{sample}"
        );
        Generator::generate(self, &prompt).await
    }
}
