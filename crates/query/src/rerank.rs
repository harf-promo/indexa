//! Cross-encoder reranking of retrieved hits.
//!
//! After hybrid retrieval + summary boosting, an optional reranking pass reorders
//! the candidates by relevance to the question. Two implementations:
//!
//! - [`LlmReranker`] — listwise rerank via the local Ollama generation model. No extra
//!   dependencies; works out-of-the-box with any model in the stack. Selected with
//!   `[retrieval] rerank_backend = "llm"` (default).
//!
//! - [`CandleReranker`] — pointwise rerank via a local DeBERTa-v2 sequence-classification
//!   model (default `mixedbread-ai/mxbai-rerank-xsmall-v1`, ~85 MB, Apache-2.0; the model is
//!   configurable via `[retrieval] rerank_model` — base/large-v1 are same-arch drop-ins).
//!   Downloaded from HuggingFace on first use and cached in `~/.cache/huggingface/hub/`. Uses
//!   pure-Rust candle for inference — no onnxruntime, no native dylib, safe for macOS notarization.
//!   Selected with `[retrieval] rerank_backend = "cross-encoder"`.
//!
//! **Reranking fails open**: it is a pure enhancement. Any parse problem, LLM error, model
//! load failure, or timeout falls back to the original hit order — reranking must never make
//! `ask` worse or produce an error.

use std::sync::OnceLock;

use anyhow::Result;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::debertav2::{
    Config as DeBertaConfig, DebertaV2SeqClassificationModel,
};
use indexa_core::store::SearchHit;
use indexa_llm::Generator;
use tokenizers::Tokenizer;

/// Reorders candidate documents by relevance to a query.
///
/// Returns best-effort 0-based indices into `docs`, most-relevant first. The
/// result may be partial, duplicated, or out-of-range — [`apply_rerank`]
/// sanitizes it, so implementations can return raw model output.
#[async_trait::async_trait]
pub(crate) trait CrossEncoder: Send + Sync {
    async fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<usize>>;
}

/// Listwise reranker backed by the local generation model. One LLM call ranks
/// all candidates at once.
pub(crate) struct LlmReranker<'a> {
    llm: &'a dyn Generator,
    /// Per-candidate snippet cap (chars) so the rerank prompt can't balloon.
    snippet_cap: usize,
}

impl<'a> LlmReranker<'a> {
    pub(crate) fn new(llm: &'a dyn Generator) -> Self {
        Self {
            llm,
            snippet_cap: 300,
        }
    }
}

#[async_trait::async_trait]
impl CrossEncoder for LlmReranker<'_> {
    async fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<usize>> {
        let mut prompt = String::with_capacity(512 + docs.len() * self.snippet_cap);
        prompt.push_str(
            "You are ranking passages by how well they help answer a question.\n\
             Return ONLY a comma-separated list of passage numbers, most relevant first \
             (e.g. `3,1,2`). Do not explain.\n\n",
        );
        prompt.push_str("Question: ");
        prompt.push_str(query);
        prompt.push_str("\n\nPassages:\n");
        for (i, doc) in docs.iter().enumerate() {
            let snippet: String = doc.chars().take(self.snippet_cap).collect();
            // 1-based for the model; converted back in parsing.
            prompt.push_str(&format!("[{}] {}\n", i + 1, snippet.replace('\n', " ")));
        }
        prompt.push_str("\nRanking (comma-separated numbers):");

        let response = self.llm.generate(&prompt).await?;
        Ok(parse_ranking(&response, docs.len()))
    }
}

// ── Candle cross-encoder ─────────────────────────────────────────────────────

/// Max tokens to feed per (query, doc) pair — model max is 512.
const MAX_SEQ_LEN: usize = 512;

struct CandleInner {
    model: DebertaV2SeqClassificationModel,
    tokenizer: Tokenizer,
    device: Device,
    /// Index of the "relevant" output logit.  1 when num_labels==2, 0 when ==1.
    score_idx: usize,
}

impl CandleInner {
    fn load(model_id: &str) -> anyhow::Result<Self> {
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.repo(hf_hub::Repo::new(
            model_id.to_string(),
            hf_hub::RepoType::Model,
        ));

        let config_path = repo.get("config.json")?;
        let tokenizer_path = repo.get("tokenizer.json")?;
        let weights_path = repo.get("model.safetensors")?;

        let cfg: DeBertaConfig = serde_json::from_str(&std::fs::read_to_string(&config_path)?)?;
        let num_labels = cfg.id2label.as_ref().map(|m| m.len()).unwrap_or(2).max(1);
        let score_idx = if num_labels == 1 { 0 } else { num_labels - 1 };

        let tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("tokenizer: {e}"))?;

        let device = Device::Cpu;
        // SAFETY: mmap is safe here — the weights file is read-only and not
        // mutated; we hold a shared reference for the lifetime of the process.
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[&weights_path], DType::F32, &device)? };
        // HF `DebertaV2ForSequenceClassification` checkpoints (all the mxbai-rerank models) nest the
        // transformer under a `deberta.` prefix, while candle's loader reads the base `DebertaV2Model`
        // at the vb root and pulls the pooler/classifier from `vb.root()`. Prefix the base model with
        // `deberta` so `deberta.embeddings.*` / `deberta.encoder.*` resolve (without it, load fails with
        // "cannot find tensor embeddings.word_embeddings.weight" and reranking silently falls open).
        let model = DebertaV2SeqClassificationModel::load(vb.pp("deberta"), &cfg, None)?;

        Ok(Self {
            model,
            tokenizer,
            device,
            score_idx,
        })
    }

    /// Score a single (query, doc) pair.  Returns NEG_INFINITY on tokenizer error.
    fn score_pair(&self, query: &str, doc: &str) -> f32 {
        (|| -> anyhow::Result<f32> {
            let enc = self
                .tokenizer
                .encode((query, doc), true)
                .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
            let len = enc.get_ids().len().min(MAX_SEQ_LEN);
            let ids: Vec<u32> = enc.get_ids()[..len].to_vec();
            let mask: Vec<u8> = enc.get_attention_mask()[..len]
                .iter()
                .map(|&x| x as u8)
                .collect();
            let type_ids: Vec<u32> = enc.get_type_ids()[..len].to_vec();

            let input_ids = Tensor::new(ids.as_slice(), &self.device)?.unsqueeze(0)?;
            let attention_mask = Tensor::new(mask.as_slice(), &self.device)?.unsqueeze(0)?;
            let token_type_ids = Tensor::new(type_ids.as_slice(), &self.device)?.unsqueeze(0)?;

            let logits =
                self.model
                    .forward(&input_ids, Some(token_type_ids), Some(attention_mask))?;
            // logits: [1, num_labels] — take the score column and squeeze to scalar.
            let score = logits.get(0)?.get(self.score_idx)?.to_scalar::<f32>()?;
            Ok(score)
        })()
        .unwrap_or(f32::NEG_INFINITY)
    }
}

/// Reranker backed by a local DeBERTa-v2 model via candle (pure Rust, CPU-only).
///
/// The model is downloaded from HuggingFace on first use and memory-mapped from
/// disk on every process start (fast — the OS page-cache keeps it warm between
/// queries). Fails open: if loading or scoring fails, `apply_rerank` returns the
/// original order unchanged.
pub(crate) struct CandleReranker {
    /// Singleton model state — `OnceLock` so the 1–2 s load cost is paid once.
    inner: &'static OnceLock<anyhow::Result<CandleInner>>,
    /// HuggingFace repo id of the DeBERTa-v2 reranker (from `[retrieval] rerank_model`).
    model_id: String,
}

// One global slot. `rerank_model` comes from config, which is fixed for the process
// lifetime, so the single slot correctly caches the one configured model — whichever id
// wins the first `get_or_init` is the only one used, and it's always the same id.
static CANDLE_INNER: OnceLock<anyhow::Result<CandleInner>> = OnceLock::new();

impl CandleReranker {
    pub(crate) fn new(model_id: &str) -> Self {
        Self {
            inner: &CANDLE_INNER,
            model_id: model_id.to_string(),
        }
    }

    fn get_inner(&self) -> anyhow::Result<&CandleInner> {
        let result = self.inner.get_or_init(|| CandleInner::load(&self.model_id));
        match result {
            Ok(inner) => Ok(inner),
            Err(e) => anyhow::bail!("candle reranker unavailable: {e:#}"),
        }
    }
}

// CandleInner: DebertaV2SeqClassificationModel and Tokenizer are both Send + Sync on CPU.
// SAFETY: candle CPU tensors + HF tokenizers are thread-safe for read-only inference.
unsafe impl Send for CandleInner {}
unsafe impl Sync for CandleInner {}

#[async_trait::async_trait]
impl CrossEncoder for CandleReranker {
    async fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<usize>> {
        // Pair strings for the blocking closure.
        let query = query.to_owned();
        let docs: Vec<String> = docs.iter().map(|s| s.to_string()).collect();

        // Get or initialize the model. Wrapping in a Mutex is not needed because
        // we own the &'static reference via OnceLock; &CandleInner is Send.
        let inner = match self.get_inner() {
            Ok(i) => i as *const CandleInner as usize, // raw pointer for Send boundary
            Err(e) => {
                tracing::warn!("candle reranker load failed, keeping original order: {e:#}");
                return Ok(Vec::new()); // apply_rerank treats empty as "keep original"
            }
        };

        tokio::task::spawn_blocking(move || {
            // SAFETY: the pointer is valid for the entire process lifetime
            // (stored in a `'static OnceLock`), and we only read from it.
            let inner = unsafe { &*(inner as *const CandleInner) };
            let mut scored: Vec<(usize, f32)> = docs
                .iter()
                .enumerate()
                .map(|(i, doc)| (i, inner.score_pair(&query, doc)))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.into_iter().map(|(i, _)| i).collect::<Vec<usize>>()
        })
        .await
        .map_err(|e| anyhow::anyhow!("candle rerank join: {e}"))
    }
}

// ── LLM listwise helper ───────────────────────────────────────────────────────

/// Extract 1-based passage numbers from a model response and convert to 0-based
/// indices. Tolerant of prose around the numbers; dedupes and drops out-of-range.
fn parse_ranking(response: &str, n: usize) -> Vec<usize> {
    let mut order = Vec::new();
    let mut seen = vec![false; n];
    let mut num = String::new();
    let flush = |num: &mut String, order: &mut Vec<usize>, seen: &mut [bool]| {
        if num.is_empty() {
            return;
        }
        if let Ok(one_based) = num.parse::<usize>() {
            if one_based >= 1 && one_based <= n {
                let idx = one_based - 1;
                if !seen[idx] {
                    seen[idx] = true;
                    order.push(idx);
                }
            }
        }
        num.clear();
    };
    for ch in response.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            flush(&mut num, &mut order, &mut seen);
        }
    }
    flush(&mut num, &mut order, &mut seen);
    order
}

/// Apply a reranker to `hits`, **failing open**. On reranker error or empty
/// result, the original order is returned unchanged. The sanitized index order
/// is completed with any hits the reranker omitted (appended in original order),
/// so no candidate is ever lost.
pub(crate) async fn apply_rerank(
    reranker: &dyn CrossEncoder,
    query: &str,
    hits: Vec<SearchHit>,
) -> Vec<SearchHit> {
    if hits.len() < 2 {
        return hits;
    }
    let docs: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
    let order = match reranker.rerank(query, &docs).await {
        Ok(o) if !o.is_empty() => o,
        Ok(_) => return hits, // empty → keep original
        Err(e) => {
            tracing::warn!("rerank failed, keeping original order: {e:#}");
            return hits;
        }
    };

    // Reorder by the sanitized index list, then append any omitted hits in
    // original order so nothing is dropped.
    let mut placed = vec![false; hits.len()];
    let mut reordered: Vec<SearchHit> = Vec::with_capacity(hits.len());
    // Move hits out into Options so each can be taken exactly once.
    let mut slots: Vec<Option<SearchHit>> = hits.into_iter().map(Some).collect();
    for &idx in &order {
        if idx < slots.len() {
            if let Some(hit) = slots[idx].take() {
                placed[idx] = true;
                reordered.push(hit);
            }
        }
    }
    for (i, slot) in slots.iter_mut().enumerate() {
        if !placed[i] {
            if let Some(hit) = slot.take() {
                reordered.push(hit);
            }
        }
    }
    reordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: i64, text: &str) -> SearchHit {
        SearchHit {
            chunk_id: id,
            entry_path: format!("/doc{id}.md"),
            seq: 0,
            heading: String::new(),
            text: text.to_owned(),
            rrf_score: 1.0,
        }
    }

    /// A stub reranker that reverses the candidate order — deterministic proof
    /// that the pipeline actually applies reranking.
    struct Reverser;
    #[async_trait::async_trait]
    impl CrossEncoder for Reverser {
        async fn rerank(&self, _q: &str, docs: &[&str]) -> Result<Vec<usize>> {
            Ok((0..docs.len()).rev().collect())
        }
    }

    /// A stub reranker that always errors — must fail open.
    struct Exploder;
    #[async_trait::async_trait]
    impl CrossEncoder for Exploder {
        async fn rerank(&self, _q: &str, _docs: &[&str]) -> Result<Vec<usize>> {
            anyhow::bail!("boom")
        }
    }

    /// A stub returning garbage (out-of-range + partial) — must sanitize + complete.
    struct Garbage;
    #[async_trait::async_trait]
    impl CrossEncoder for Garbage {
        async fn rerank(&self, _q: &str, _docs: &[&str]) -> Result<Vec<usize>> {
            // only index 2, plus out-of-range 99 and a dupe
            Ok(vec![2, 99, 2])
        }
    }

    #[tokio::test]
    async fn reverser_flips_order() {
        let hits = vec![hit(0, "a"), hit(1, "b"), hit(2, "c")];
        let out = apply_rerank(&Reverser, "q", hits).await;
        let ids: Vec<i64> = out.iter().map(|h| h.chunk_id).collect();
        assert_eq!(ids, vec![2, 1, 0]);
    }

    #[tokio::test]
    async fn error_keeps_original_order() {
        let hits = vec![hit(0, "a"), hit(1, "b"), hit(2, "c")];
        let out = apply_rerank(&Exploder, "q", hits).await;
        let ids: Vec<i64> = out.iter().map(|h| h.chunk_id).collect();
        assert_eq!(ids, vec![0, 1, 2]); // unchanged — failed open
    }

    #[tokio::test]
    async fn garbage_is_sanitized_and_completed() {
        let hits = vec![hit(0, "a"), hit(1, "b"), hit(2, "c")];
        let out = apply_rerank(&Garbage, "q", hits).await;
        let ids: Vec<i64> = out.iter().map(|h| h.chunk_id).collect();
        // index 2 first (valid), 99 dropped, dupe ignored, then 0 and 1 appended in order.
        assert_eq!(ids, vec![2, 0, 1]);
    }

    #[test]
    fn parse_ranking_tolerates_prose() {
        // "most relevant: 3, then 1, then 2" → [2,0,1] (0-based)
        assert_eq!(
            parse_ranking("most relevant: 3, then 1, then 2", 3),
            vec![2, 0, 1]
        );
        // out-of-range and dupes dropped
        assert_eq!(parse_ranking("3,3,9,1", 3), vec![2, 0]);
        // no numbers → empty (caller fails open)
        assert!(parse_ranking("I cannot rank these", 3).is_empty());
    }

    /// Proves the config-supplied `[retrieval] rerank_model` id loads through the DeBERTa path
    /// and scores. `#[ignore]`d — downloads the model from HuggingFace on first run.
    ///
    /// ```bash
    /// cargo test -p indexa-query candle_reranker_loads -- --ignored --nocapture
    /// # or point at a bigger variant:
    /// #   (edit the id below to mixedbread-ai/mxbai-rerank-base-v1)
    /// ```
    #[tokio::test]
    #[ignore = "needs a HuggingFace model download (~85 MB for xsmall) + network"]
    async fn candle_reranker_loads_configured_model() {
        // The config default id, or any variant via INDEXA_TEST_RERANK_MODEL (e.g. base-v1).
        let model = std::env::var("INDEXA_TEST_RERANK_MODEL")
            .unwrap_or_else(|_| indexa_core::config::RetrievalConfig::default().rerank_model);
        let reranker = CandleReranker::new(&model);
        let docs = [
            "The mitochondria is the powerhouse of the cell.",
            "Tokio is an asynchronous runtime for Rust with a work-stealing scheduler.",
            "Sourdough bread relies on a wild-yeast starter.",
        ];
        let order = reranker
            .rerank("how does the tokio async scheduler work", &docs)
            .await
            .expect("rerank should not error");
        // A successful load returns a full permutation; a load FAILURE returns an empty vec
        // (fail-open), so a non-empty full permutation is the proof the model actually loaded.
        assert_eq!(
            order.len(),
            docs.len(),
            "empty order = model failed to load (fail-open) for id {model}"
        );
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2], "must be a permutation of all docs");
        // Quality sanity: the Tokio doc is the obvious match → should rank first.
        assert_eq!(
            order[0], 1,
            "expected the tokio doc top-ranked, got {order:?}"
        );
    }
}
