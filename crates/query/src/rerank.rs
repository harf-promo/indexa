//! Cross-encoder reranking of retrieved hits.
//!
//! After hybrid retrieval + summary boosting, an optional reranking pass reorders
//! the candidates by relevance to the question. The default implementation
//! ([`LlmReranker`]) does a single listwise LLM call — no new native dependency,
//! works on the local Ollama model, and is gated behind `QaConfig.rerank`
//! (default off). A future `fastembed`/ONNX cross-encoder can implement the same
//! [`CrossEncoder`] trait behind a Cargo feature.
//!
//! **Reranking fails open**: it is a pure enhancement. Any parse problem, LLM
//! error, or timeout falls back to the original hit order — reranking must never
//! make `ask` worse or error it.

use anyhow::Result;
use indexa_core::store::SearchHit;
use indexa_llm::Generator;

/// Reorders candidate documents by relevance to a query.
///
/// Returns best-effort 0-based indices into `docs`, most-relevant first. The
/// result may be partial, duplicated, or out-of-range — [`apply_rerank`]
/// sanitizes it, so implementations can return raw model output.
#[async_trait::async_trait]
pub trait CrossEncoder: Send + Sync {
    async fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<usize>>;
}

/// Listwise reranker backed by the local generation model. One LLM call ranks
/// all candidates at once.
pub struct LlmReranker<'a> {
    llm: &'a dyn Generator,
    /// Per-candidate snippet cap (chars) so the rerank prompt can't balloon.
    snippet_cap: usize,
}

impl<'a> LlmReranker<'a> {
    pub fn new(llm: &'a dyn Generator) -> Self {
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
pub async fn apply_rerank(
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
}
