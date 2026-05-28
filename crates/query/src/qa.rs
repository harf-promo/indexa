use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{SearchHit, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;

/// Result of a Q&A query.
#[derive(Debug)]
pub struct Answer {
    pub question: String,
    pub answer: String,
    pub sources: Vec<SourceCitation>,
}

#[derive(Debug)]
pub struct SourceCitation {
    pub path: String,
    pub heading: String,
    pub snippet: String,
}

/// Configuration for the Q&A pipeline.
pub struct QaConfig {
    pub top_k: usize,
    /// Max characters of context to include in the LLM prompt.
    pub context_budget: usize,
    /// Retrieval mode (RRF / sparse / dense).
    pub mode: HybridMode,
    /// Limit search to paths starting with this prefix (tilde-expanded).
    pub scope: Option<String>,
    /// RRF rank constant (industry default: 60).
    pub rrf_k: f32,
}

impl Default for QaConfig {
    fn default() -> Self {
        Self {
            top_k: 8,
            context_budget: 4000,
            mode: HybridMode::Rrf,
            scope: None,
            rrf_k: 60.0,
        }
    }
}

/// Run the full RAG Q&A pipeline:
///   embed(query) → hybrid_search → pack context → LLM → cited answer.
///
/// The store query is synchronous and completes before any async calls,
/// so this function never holds `&Store` across an `.await` point.
pub async fn ask(
    store: &Store,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
) -> Result<Answer> {
    // 1. Embed the question (skip if sparse-only).
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(question).await?),
    };

    // 2. Hybrid retrieval (sync — no await while holding &store).
    let scope = cfg.scope.as_deref();
    let hits = store.hybrid_search(
        question,
        query_vec.as_deref(),
        &cfg.mode,
        scope,
        cfg.top_k,
        cfg.rrf_k,
    )?;

    // 3–5. Synthesize (no store access from here on).
    synthesize_from_hits(hits, llm, question, cfg).await
}

/// Synthesise an answer from pre-retrieved hits (no store access).
/// Use this when the caller already has hits and wants to avoid holding
/// a non-Sync store lock across async boundaries.
pub async fn synthesize_from_hits(
    hits: Vec<SearchHit>,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
) -> Result<Answer> {
    let (context, sources) = pack_context(&hits, cfg.context_budget);
    let prompt = build_prompt(question, &context);
    let answer_text = llm.generate(&prompt).await?;
    Ok(Answer {
        question: question.to_owned(),
        answer: answer_text.trim().to_owned(),
        sources,
    })
}

fn pack_context(hits: &[SearchHit], budget: usize) -> (String, Vec<SourceCitation>) {
    let mut context = String::new();
    let mut sources = Vec::new();
    let mut chars_used = 0;

    for (i, hit) in hits.iter().enumerate() {
        let header = if hit.heading.is_empty() {
            format!("[{}] {}\n", i + 1, hit.entry_path)
        } else {
            format!("[{}] {} — {}\n", i + 1, hit.entry_path, hit.heading)
        };
        let chunk = format!("{}{}\n\n", header, hit.text);

        if chars_used + chunk.len() > budget {
            let remaining = budget.saturating_sub(chars_used);
            if remaining > header.len() + 40 {
                let truncated = &chunk[..remaining];
                context.push_str(truncated);
                context.push_str("...\n\n");
            }
            break;
        }
        chars_used += chunk.len();
        context.push_str(&chunk);

        sources.push(SourceCitation {
            path: hit.entry_path.clone(),
            heading: hit.heading.clone(),
            snippet: hit.text.chars().take(120).collect::<String>() + "...",
        });
    }

    (context, sources)
}

fn build_prompt(question: &str, context: &str) -> String {
    format!(
        "You are a helpful assistant that answers questions about files on a computer.\n\
         Use ONLY the provided context to answer. Cite sources by their [number].\n\
         If the answer isn't in the context, say so.\n\
         \n\
         CONTEXT:\n\
         {context}\n\
         \n\
         QUESTION: {question}\n\
         \n\
         ANSWER:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pack_context_truncates_to_budget() {
        let hits: Vec<SearchHit> = (0..5)
            .map(|i| SearchHit {
                chunk_id: i,
                entry_path: format!("/doc{i}.md"),
                seq: 0,
                heading: String::new(),
                text: "a".repeat(1000),
                rrf_score: 1.0 / (i as f64 + 1.0),
            })
            .collect();

        let (ctx, sources) = pack_context(&hits, 2000);
        assert!(ctx.len() <= 2100);
        assert!(!sources.is_empty());
    }

    #[test]
    fn build_prompt_contains_question_and_context() {
        let prompt = build_prompt("what is 2+2?", "some context");
        assert!(prompt.contains("what is 2+2?"));
        assert!(prompt.contains("some context"));
    }
}
