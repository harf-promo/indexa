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

/// Batch request: the OpenAI `/v1/embeddings` endpoint accepts `input` as an array.
#[derive(Serialize)]
struct EmbedBatchRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbedBatchResponse {
    data: Vec<EmbedBatchItem>,
}

#[derive(Deserialize)]
struct EmbedBatchItem {
    embedding: Vec<f32>,
    /// Position in the request `input` array. The API may return items out of order, so we
    /// MUST realign by this rather than trust response order — otherwise a chunk could be
    /// stored against another chunk's vector. (`default` so a server that omits it doesn't
    /// hard-fail parse; `order_embeddings` then rejects the non-contiguous result → fallback.)
    #[serde(default)]
    index: usize,
}

/// Realign `(index, embedding)` items to input order, validating completeness. Returns `None`
/// (→ caller falls back to sequential) unless the items form exactly one vector per input slot
/// `0..n`, each of dimension `dim`. This is the single guard against a batch silently
/// misaligning chunks with the wrong vectors, so it is strict by design and unit-tested.
fn order_embeddings(items: Vec<(usize, Vec<f32>)>, n: usize, dim: usize) -> Option<Vec<Vec<f32>>> {
    if items.len() != n {
        return None;
    }
    let mut slots: Vec<Option<Vec<f32>>> = (0..n).map(|_| None).collect();
    for (idx, emb) in items {
        if idx >= n || emb.len() != dim || slots[idx].is_some() {
            return None; // out of range, wrong dim, or a duplicate index
        }
        slots[idx] = Some(emb);
    }
    slots.into_iter().collect() // None if any slot is still empty
}

#[async_trait::async_trait]
impl Embedder for OpenAIEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbedRequest {
            model: &self.model,
            input: text,
        };

        let resp = crate::send_with_retry(
            || {
                let mut b = self.client.post(&url).json(&body);
                if !self.api_key.is_empty() {
                    b = b.bearer_auth(&self.api_key);
                }
                b
            },
            2,
        )
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

    /// One round-trip for the whole sub-batch (the deep-phase speedup) using the API's array
    /// `input`. Mirrors the Ollama adapter: any failure — network, non-2xx, parse, or a
    /// count/dim/index mismatch caught by [`order_embeddings`] — **falls open** to sequential
    /// [`embed`](Self::embed), so a batch can never corrupt or drop a file's embeddings; the
    /// worst case is no speedup. Results are realigned to input order by each item's `index`.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbedBatchRequest {
            model: &self.model,
            input: texts,
        };
        let attempt: Result<Vec<Vec<f32>>> = async {
            let resp = crate::send_with_retry(
                || {
                    let mut b = self.client.post(&url).json(&body);
                    if !self.api_key.is_empty() {
                        b = b.bearer_auth(&self.api_key);
                    }
                    b
                },
                2,
            )
            .await
            .with_context(|| format!("OpenAI batch embeddings request to {url}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("OpenAI embeddings returned {status}: {body}");
            }
            let parsed: EmbedBatchResponse = resp
                .json()
                .await
                .context("parsing OpenAI batch embeddings response")?;
            let items = parsed
                .data
                .into_iter()
                .map(|i| (i.index, i.embedding))
                .collect();
            order_embeddings(items, texts.len(), self.dim)
                .context("OpenAI batch response had a wrong count/dim/index")
        }
        .await;

        match attempt {
            Ok(embeddings) => Ok(embeddings),
            Err(e) => {
                tracing::debug!("OpenAI batch embed failed ({e:#}); falling back to sequential");
                let mut out = Vec::with_capacity(texts.len());
                for t in texts {
                    out.push(self.embed(t).await?);
                }
                Ok(out)
            }
        }
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

    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY env var and network access"]
    async fn live_embed_batch_matches_sequential() {
        // Confirms the array request + index realignment against the real API: each batched
        // vector must equal the sequential embed of the same text, in order.
        let e = OpenAIEmbedder::from_env(DEFAULT_MODEL, DEFAULT_DIM).unwrap();
        let texts = ["alpha one", "beta two", "gamma three"];
        let batched = e.embed_batch(&texts).await.unwrap();
        assert_eq!(batched.len(), 3);
        for (i, t) in texts.iter().enumerate() {
            assert_eq!(batched[i].len(), DEFAULT_DIM);
            let seq = e.embed(t).await.unwrap();
            assert_eq!(
                batched[i], seq,
                "batch[{i}] must match sequential embed of {t:?}"
            );
        }
    }

    #[test]
    fn order_embeddings_realigns_and_rejects_bad_responses() {
        let v = |x: f32| vec![x, x]; // dim = 2
                                     // In order.
        assert_eq!(
            order_embeddings(vec![(0, v(0.0)), (1, v(1.0))], 2, 2),
            Some(vec![v(0.0), v(1.0)])
        );
        // Out of order → realigned to input order.
        assert_eq!(
            order_embeddings(vec![(1, v(1.0)), (0, v(0.0))], 2, 2),
            Some(vec![v(0.0), v(1.0)])
        );
        // Wrong count → None (→ caller falls back).
        assert_eq!(order_embeddings(vec![(0, v(0.0))], 2, 2), None);
        // Duplicate index → None.
        assert_eq!(order_embeddings(vec![(0, v(0.0)), (0, v(1.0))], 2, 2), None);
        // Index out of range → None.
        assert_eq!(order_embeddings(vec![(0, v(0.0)), (5, v(1.0))], 2, 2), None);
        // Wrong dim → None (never store a mis-sized vector).
        assert_eq!(
            order_embeddings(vec![(0, vec![1.0]), (1, v(1.0))], 2, 2),
            None
        );
    }
}
