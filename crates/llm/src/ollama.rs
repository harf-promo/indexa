use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default generation model — users override via config.toml `[describer] model = "..."`.
pub const DEFAULT_MODEL: &str = "qwen2.5:14b";

pub struct OllamaLlm {
    base_url: String,
    model: String,
    client: reqwest::Client,
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

    /// Generate a completion for `prompt`. Returns the full response text.
    pub async fn generate(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);

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
impl crate::Describer for OllamaLlm {
    async fn describe(&self, path: &str, content_sample: &[u8]) -> Result<String> {
        let sample = std::str::from_utf8(content_sample)
            .unwrap_or("[binary]")
            .chars()
            .take(500)
            .collect::<String>();
        let prompt = format!(
            "Briefly describe what this file is about in 1-2 sentences.\nFile: {path}\nContent:\n{sample}"
        );
        self.generate(&prompt).await
    }
}
