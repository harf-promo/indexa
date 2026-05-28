use crate::{ChildSummary, Describer, Generator};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default generation model — users override via config.toml `[describer] model = "..."`.
/// gemma3:12b (Google, Apache-2.0) — strong on summarization/RAG, ~8GB download.
pub const DEFAULT_MODEL: &str = "gemma3:12b";
/// Smaller model used for per-file descriptions by default.
pub const DEFAULT_FILE_MODEL: &str = "gemma3:4b";

const DEFAULT_BASE: &str = "http://localhost:11434";

pub struct OllamaLlm {
    pub(crate) base_url: String,
    pub(crate) model: String,
    /// Model used for `summarize_dir`; falls back to `model` when None.
    pub(crate) dir_model: Option<String>,
    pub(crate) client: reqwest::Client,
}

impl OllamaLlm {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dir_model: None,
            client: reqwest::Client::new(),
        }
    }

    /// Construct with separate models for file descriptions and directory roll-ups.
    pub fn new_with_dir_model(
        base_url: impl Into<String>,
        file_model: impl Into<String>,
        dir_model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: file_model.into(),
            dir_model: Some(dir_model.into()),
            client: reqwest::Client::new(),
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
        Self::new(Self::resolve_base_url(None), DEFAULT_MODEL)
    }

    fn effective_dir_model(&self) -> &str {
        self.dir_model.as_deref().unwrap_or(&self.model)
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
    async fn describe(
        &self,
        path: &str,
        content_sample: &[u8],
        previous_summary: Option<&str>,
    ) -> Result<String> {
        let sample = std::str::from_utf8(content_sample)
            .unwrap_or("[binary]")
            .chars()
            .take(800)
            .collect::<String>();
        let prompt = match previous_summary {
            None => format!(
                "Briefly describe what this file is about in 1-2 sentences.\nFile: {path}\nContent:\n{sample}"
            ),
            Some(prev) => format!(
                "We have provided an existing summary up to a certain point:\n{prev}\n\n\
                 We have the opportunity to refine the existing summary (only if needed) \
                 with some more context below.\n\
                 File: {path}\nContent:\n{sample}\n\n\
                 Given the new context, refine the original summary. \
                 If the context isn't useful, return the original summary."
            ),
        };
        Generator::generate(self, &prompt).await
    }

    async fn summarize_dir(
        &self,
        dir_path: &str,
        children: &[ChildSummary],
        previous_summary: Option<&str>,
    ) -> Result<String> {
        let n_files = children.iter().filter(|c| c.kind == "file").count();
        let n_dirs = children.iter().filter(|c| c.kind == "dir").count();

        let bullets = children
            .iter()
            .take(30)
            .map(|c| {
                let icon = if c.kind == "dir" { "📁" } else { "📄" };
                format!("  {icon} {}: {}", c.name, c.summary)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let base_desc = format!(
            "You are describing a folder so a future search can understand its purpose.\n\
             Folder: {dir_path}\n\
             Direct children ({n_files} files, {n_dirs} subfolders):\n\
             {bullets}\n\n\
             Write 2-4 sentences capturing: (1) what this folder is for, \
             (2) the kinds of work or content inside, (3) anything notable. \
             Do not list filenames. Speak about themes."
        );
        let prompt = match previous_summary {
            None => base_desc,
            Some(prev) => format!(
                "We have provided an existing summary up to a certain point:\n{prev}\n\n\
                 We have the opportunity to refine the existing summary (only if needed) \
                 with some more context below.\n{base_desc}\n\n\
                 Given the new context, refine the original summary. \
                 If the context isn't useful, return the original summary."
            ),
        };

        // Use the dedicated dir model if configured
        let model = self.effective_dir_model().to_owned();
        let url = format!("{}/api/generate", self.base_url);
        let body = Req {
            model: &model,
            prompt: &prompt,
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
