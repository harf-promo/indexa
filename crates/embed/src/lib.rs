//! Embedding adapter — trait + BYO-model implementations.
//!
//! Supported providers:
//! - `ollama`   — Ollama local server (`/api/embeddings`), default nomic-embed-text
//! - `openai`   — OpenAI `/v1/embeddings` API (requires `OPENAI_API_KEY`)
//! - `llamacpp` — llama.cpp HTTP server (OpenAI-compatible `/v1/embeddings`)
//! - `google`   — Google Gemini `/v1beta/models/:embedContent` (requires `GOOGLE_API_KEY`)

pub mod google;
pub mod ollama;
pub mod openai;

pub use google::GoogleEmbedder;
pub use ollama::OllamaEmbedder;
pub use openai::OpenAIEmbedder;

/// Build a reqwest client with a finite request + connect timeout, shared by every
/// embedding adapter. Without this a stalled cloud endpoint (no FIN, no bytes) would hang
/// `embed()` forever — and these run inside the indexing worker and web/MCP request paths.
/// `expect` is appropriate: `build()` only fails if the rustls TLS backend can't initialize,
/// which is unrecoverable at startup (and never silently yields a no-timeout client, unlike
/// the old `.unwrap_or_default()`).
pub(crate) fn http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("building reqwest client (rustls TLS init)")
}

/// HTTP status codes worth retrying — transient server errors and rate limits.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Backoff before retry `attempt` (0-based): honor `Retry-After` if present (capped at 30s),
/// otherwise exponential `0.5s · 2^attempt`, capped at 8s.
pub(crate) fn backoff_delay(
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> std::time::Duration {
    use std::time::Duration;
    if let Some(ra) = retry_after {
        return ra.min(Duration::from_secs(30));
    }
    (Duration::from_millis(500) * 2u32.saturating_pow(attempt)).min(Duration::from_secs(8))
}

/// Send a freshly-built request with bounded retries on transient failures (retryable status
/// codes + connection/timeout errors). `build` is called once per attempt because `send()`
/// consumes the builder. Bulk indexing routinely hits 429/503 from cloud providers; without
/// this each such response permanently fails that file's embedding/summary.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    max_retries: u32,
) -> reqwest::Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        match build().send().await {
            Ok(resp) if attempt < max_retries && is_retryable_status(resp.status().as_u16()) => {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);
                tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
                attempt += 1;
            }
            Ok(resp) => return Ok(resp),
            Err(e)
                if attempt < max_retries
                    && (e.is_timeout() || e.is_connect() || e.is_request()) =>
            {
                tokio::time::sleep(backoff_delay(attempt, None)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Produces a vector embedding for a piece of text.
/// `dim()` reports the vector length so callers can allocate the right buffer.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn dim(&self) -> usize;

    /// Best-effort: free any resident model so RAM can recover during a memory-pressure
    /// pause. The default is a no-op (cloud adapters hold no local memory); Ollama
    /// overrides this to unload its loaded model.
    async fn unload(&self) {}
}

/// Build an `Embedder` from config values, optionally setting `keep_alive` on Ollama adapters.
///
/// `openai_key` / `google_key` are used as fallbacks when the corresponding
/// environment variables (`OPENAI_API_KEY`, `GOOGLE_API_KEY`) are not set.
/// Pass `None` to require the env var.
// One factory that fans config fields out to the right provider constructor; grouping these
// into a struct would just move the same fields around without improving clarity.
#[allow(clippy::too_many_arguments)]
pub fn from_config_with_keep_alive(
    provider: &str,
    model: &str,
    dim: usize,
    base_url: &str,
    openai_key: Option<&str>,
    google_key: Option<&str>,
    keep_alive: Option<i64>,
    num_ctx: u32,
) -> anyhow::Result<Box<dyn Embedder + Send + Sync>> {
    let base = if base_url.is_empty() {
        None
    } else {
        Some(base_url)
    };
    match provider {
        "ollama" => {
            let url = OllamaEmbedder::resolve_base_url(base);
            let embedder = match keep_alive {
                Some(ka) => OllamaEmbedder::new_with_keep_alive(url, model, dim, ka),
                None => OllamaEmbedder::new(url, model, dim),
            }
            .with_num_ctx(num_ctx);
            Ok(Box::new(embedder))
        }
        "openai" => Ok(Box::new(OpenAIEmbedder::from_env_or_config(
            model, dim, openai_key, base,
        )?)),
        "llamacpp" => Ok(Box::new(OpenAIEmbedder::local_llamacpp(
            OpenAIEmbedder::resolve_base_url(base),
            model,
            dim,
        ))),
        "google" => Ok(Box::new(GoogleEmbedder::from_env_or_config(
            model, dim, google_key, base,
        )?)),
        other => anyhow::bail!("unknown embedding provider: {other}"),
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn retryable_status_classification() {
        for s in [408, 425, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(s), "{s} should retry");
        }
        for s in [200, 201, 400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(s), "{s} should not retry");
        }
    }

    #[test]
    fn backoff_exponential_and_capped() {
        assert!(backoff_delay(0, None) < backoff_delay(2, None));
        assert!(backoff_delay(20, None) <= Duration::from_secs(8));
        // Retry-After honored and capped at 30s.
        assert_eq!(
            backoff_delay(0, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(
            backoff_delay(5, Some(Duration::from_secs(120))),
            Duration::from_secs(30)
        );
    }
}
