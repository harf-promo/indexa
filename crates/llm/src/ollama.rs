use crate::{ChildSummary, Describer, Generator};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

/// Default generation model — users override via config.toml `[describer] model = "..."`.
/// gemma3:12b (Google, Apache-2.0) — strong on summarization/RAG, ~8GB download.
pub const DEFAULT_MODEL: &str = "gemma3:12b";
/// Smaller model used for per-file descriptions by default.
pub const DEFAULT_FILE_MODEL: &str = "gemma3:4b";

const DEFAULT_BASE: &str = "http://localhost:11434";

/// Timeout for LLM generate requests.
/// 3 minutes is generous even for a slow local model on a large file.
const LLM_TIMEOUT_SECS: u64 = 180;

fn ollama_client() -> reqwest::Client {
    crate::http_client(LLM_TIMEOUT_SECS)
}

pub struct OllamaLlm {
    pub(crate) base_url: String,
    pub(crate) model: String,
    /// Model used for `summarize_dir`; falls back to `model` when None.
    pub(crate) dir_model: Option<String>,
    pub(crate) client: reqwest::Client,
    /// keep_alive value to send with every request (seconds).
    /// 0 = unload immediately; -1 = keep forever; None = server default.
    pub(crate) keep_alive: Option<i64>,
}

impl OllamaLlm {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dir_model: None,
            client: ollama_client(),
            keep_alive: None,
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
            client: ollama_client(),
            keep_alive: None,
        }
    }

    /// Construct with separate models and an explicit keep_alive (seconds).
    pub fn new_with_keep_alive(
        base_url: impl Into<String>,
        file_model: impl Into<String>,
        dir_model: Option<String>,
        keep_alive: i64,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: file_model.into(),
            dir_model,
            client: ollama_client(),
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
        Self::new(Self::resolve_base_url(None), DEFAULT_MODEL)
    }

    fn effective_dir_model(&self) -> &str {
        self.dir_model.as_deref().unwrap_or(&self.model)
    }

    /// Stream generation with an explicit model name — used by `summarize_dir_stream`
    /// which needs the dir model, not `self.model`.
    async fn stream_with_model(
        &self,
        model: &str,
        prompt: &str,
        on_fragment: &mut (dyn FnMut(String) + Send),
    ) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);
        let body = Req {
            model,
            prompt,
            stream: true,
            keep_alive: self.keep_alive,
            options: Some(GenOptions { num_parallel: 1 }),
        };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Ollama streaming request to {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {text}");
        }
        let mut stream = resp.bytes_stream();
        let mut full = String::new();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading Ollama stream chunk")?;
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes = buf.drain(..=pos).collect::<Vec<u8>>();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(sc) = serde_json::from_str::<StreamChunk>(line) {
                    if let Some(err) = sc.error {
                        anyhow::bail!("Ollama stream error: {err}");
                    }
                    if !sc.response.is_empty() {
                        full.push_str(&sc.response);
                        on_fragment(sc.response);
                    }
                    if sc.done {
                        return Ok(full);
                    }
                }
            }
        }
        // The stream closed without a final `done: true` — treat the partial buffer as a
        // truncated/incomplete response rather than reporting it as a successful answer.
        anyhow::bail!("Ollama stream ended without completion (done=true never received)")
    }

    /// Explicitly unload a model from Ollama by sending keep_alive=0.
    /// Best-effort: errors are logged but not propagated.
    pub async fn unload(&self, model: &str) {
        let url = format!("{}/api/generate", self.base_url);
        let body = serde_json::json!({
            "model": model,
            "prompt": "",
            "stream": false,
            "keep_alive": 0
        });
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            tracing::warn!("Failed to unload model '{model}' from Ollama: {e}");
        }
    }

    /// Unload all models this instance may have loaded (file model + dir model).
    pub async fn unload_all(&self) {
        self.unload(&self.model).await;
        if let Some(ref dm) = self.dir_model {
            if dm != &self.model {
                self.unload(dm).await;
            }
        }
    }
}

/// Generation options forwarded to Ollama.
#[derive(Serialize)]
struct GenOptions {
    /// Lock to 1 parallel slot to prevent KV-cache size multiplication.
    num_parallel: u32,
}

#[derive(Serialize)]
struct Req<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
    /// Seconds to keep model loaded after this call (0 = unload immediately).
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<i64>,
    /// Inference options: pin num_parallel=1 to avoid KV-cache explosion.
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<GenOptions>,
}

#[derive(Deserialize)]
struct Resp {
    #[serde(default)]
    response: String,
    /// Ollama can return HTTP 200 with an `error` field (e.g. "model requires more system
    /// memory…"); surfacing it prevents reporting an empty answer as success.
    #[serde(default)]
    error: Option<String>,
}

/// One NDJSON line from Ollama's streaming `/api/generate` response.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    response: String,
    #[serde(default)]
    done: bool,
    /// Mid-stream error line (HTTP 200, then an `{"error": …}` object).
    #[serde(default)]
    error: Option<String>,
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
            keep_alive: self.keep_alive,
            options: Some(GenOptions { num_parallel: 1 }),
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
        if let Some(err) = parsed.error {
            anyhow::bail!("Ollama returned an error: {err}");
        }
        Ok(parsed.response)
    }

    /// Streaming variant: uses Ollama's NDJSON stream (`"stream": true`).
    /// Calls `on_fragment` with each token/chunk as it arrives, returns full text.
    async fn generate_stream(
        &self,
        prompt: &str,
        on_fragment: &mut (dyn FnMut(String) + Send),
    ) -> Result<String> {
        let url = format!("{}/api/generate", self.base_url);
        let body = Req {
            model: &self.model,
            prompt,
            stream: true,
            keep_alive: self.keep_alive,
            options: Some(GenOptions { num_parallel: 1 }),
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Ollama streaming request to {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {text}");
        }

        let mut stream = resp.bytes_stream();
        let mut full = String::new();
        let mut buf = Vec::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading Ollama stream chunk")?;
            buf.extend_from_slice(&bytes);

            // NDJSON: each complete line is one JSON object.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes = buf.drain(..=pos).collect::<Vec<u8>>();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(sc) = serde_json::from_str::<StreamChunk>(line) {
                    if let Some(err) = sc.error {
                        anyhow::bail!("Ollama stream error: {err}");
                    }
                    if !sc.response.is_empty() {
                        full.push_str(&sc.response);
                        on_fragment(sc.response); // pass owned fragment
                    }
                    if sc.done {
                        return Ok(full);
                    }
                }
            }
        }

        anyhow::bail!("Ollama stream ended without completion (done=true never received)")
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

    /// Streaming override: builds the same prompt as `describe` but streams via NDJSON.
    async fn describe_stream(
        &self,
        path: &str,
        content_sample: &[u8],
        previous_summary: Option<&str>,
        on_fragment: &mut (dyn FnMut(String) + Send),
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
        Generator::generate_stream(self, &prompt, on_fragment).await
    }

    /// Streaming override: builds the same prompt as `summarize_dir` but uses the dir model
    /// and streams each token via `on_fragment`.
    async fn summarize_dir_stream(
        &self,
        dir_path: &str,
        children: &[ChildSummary],
        previous_summary: Option<&str>,
        on_fragment: &mut (dyn FnMut(String) + Send),
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
        let model = self.effective_dir_model().to_owned();
        self.stream_with_model(&model, &prompt, on_fragment).await
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
            keep_alive: self.keep_alive,
            options: Some(GenOptions { num_parallel: 1 }),
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
        if let Some(err) = parsed.error {
            anyhow::bail!("Ollama returned an error: {err}");
        }
        Ok(parsed.response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resp_captures_error_field() {
        // Ollama can return HTTP 200 with an error body (e.g. OOM). The wire struct must
        // surface it so generate() can bail instead of returning an empty success.
        let r: Resp =
            serde_json::from_str(r#"{"error":"model requires more system memory"}"#).unwrap();
        assert_eq!(
            r.error.as_deref(),
            Some("model requires more system memory")
        );
        assert!(r.response.is_empty());

        let ok: Resp = serde_json::from_str(r#"{"response":"hello"}"#).unwrap();
        assert!(ok.error.is_none());
        assert_eq!(ok.response, "hello");
    }

    #[test]
    fn stream_chunk_captures_error_and_done() {
        let err: StreamChunk = serde_json::from_str(r#"{"error":"boom"}"#).unwrap();
        assert_eq!(err.error.as_deref(), Some("boom"));

        let done: StreamChunk = serde_json::from_str(r#"{"response":"x","done":true}"#).unwrap();
        assert!(done.error.is_none());
        assert!(done.done);
    }
}
