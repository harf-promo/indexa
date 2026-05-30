use std::path::Path;

use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{SearchHit, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;

use crate::rerank::{apply_rerank, LlmReranker};

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
    /// Weight applied to parent-directory summary similarity boost (0.0 = disabled).
    pub summary_weight: f32,
    /// Depth-boost coefficient α for summary cosine search.
    pub summary_depth_alpha: f32,
    /// Apply a cross-encoder rerank pass after retrieval (default off). Currently
    /// a local LLM-listwise reranker; fails open (never errors `ask`).
    pub rerank: bool,
}

impl Default for QaConfig {
    fn default() -> Self {
        Self {
            top_k: 8,
            context_budget: 4000,
            mode: HybridMode::Rrf,
            scope: None,
            rrf_k: 60.0,
            summary_weight: 0.0,
            summary_depth_alpha: 0.15,
            rerank: false,
        }
    }
}

/// Synchronous retrieval: hybrid search + summary boost. Kept separate so the
/// async orchestrator ([`answer`]) can scope the `&Store` borrow to a block that
/// never spans an `.await` — keeping the resulting future `Send` (required by the
/// axum web server and the rmcp MCP server). `query_vec` is `None` for sparse-only.
pub fn retrieve(
    store: &Store,
    question: &str,
    query_vec: Option<&[f32]>,
    cfg: &QaConfig,
) -> Result<Vec<SearchHit>> {
    let mut hits = store.hybrid_search(
        question,
        query_vec,
        &cfg.mode,
        cfg.scope.as_deref(),
        cfg.top_k,
        cfg.rrf_k,
    )?;
    if let Some(qvec) = query_vec {
        let _ = store.boost_with_summaries(
            &mut hits,
            qvec,
            cfg.summary_weight,
            cfg.summary_depth_alpha,
        );
    }
    Ok(hits)
}

/// Run the full RAG Q&A pipeline against the index at `db_path`:
///   embed(query) → retrieve → [rerank] → synthesize → cited answer.
///
/// **Send-safe and the single entry point** for all surfaces (CLI, web, MCP).
/// The `&Store` is confined to a synchronous inner scope and dropped before any
/// `.await`, so the returned future is `Send`. Opening a fresh connection per
/// call is cheap (sub-millisecond) and avoids holding a lock across the LLM round-trips.
pub async fn answer(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
) -> Result<Answer> {
    // 1. Embed (no store in scope). Skip for sparse-only.
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(question).await?),
    };

    // 2. Retrieve in a sync scope — `&Store` never crosses an await.
    let hits = {
        let store = Store::open(db_path)?;
        retrieve(&store, question, query_vec.as_deref(), cfg)?
    };

    // 2b. Short-circuit on no matches: with zero grounding the LLM would
    //     hallucinate a confident answer. Tell the user to index instead.
    //     Centralised here so CLI, web, and MCP all behave the same.
    if hits.is_empty() {
        return Ok(Answer {
            question: question.to_owned(),
            answer: "No indexed content matched your query. Run `indexa deep` and \
                     `indexa summarize` on the relevant folder first, then ask again."
                .to_owned(),
            sources: Vec::new(),
        });
    }

    // 3. Optional cross-encoder rerank (fails open). Reaches every surface
    //    because they all call this function.
    let hits = if cfg.rerank {
        apply_rerank(&LlmReranker::new(llm), question, hits).await
    } else {
        hits
    };

    // 4. Synthesize (no store access).
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
                // Walk back to the nearest char boundary so we never slice mid-codepoint
                // (slicing a String by a raw byte offset panics on any non-ASCII content:
                // accented chars, CJK, emoji, em-dashes, etc.). `floor_char_boundary` is
                // still nightly-only, so do it manually with is_char_boundary.
                let mut safe_end = remaining.min(chunk.len());
                while safe_end > 0 && !chunk.is_char_boundary(safe_end) {
                    safe_end -= 1;
                }
                context.push_str(&chunk[..safe_end]);
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

    // ── answer() unified-pipeline tests (CLI/web/MCP all call this) ────────────
    use indexa_core::store::ChunkRecord;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Embedder that counts calls — lets us assert Sparse mode never embeds.
    struct CountingEmbedder {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Embedder for CountingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![0.1, 0.2, 0.3])
        }
        fn dim(&self) -> usize {
            3
        }
    }

    /// Generator that counts calls and returns a fixed reply.
    struct CountingGen {
        calls: Arc<AtomicUsize>,
        reply: String,
    }
    #[async_trait::async_trait]
    impl Generator for CountingGen {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.reply.clone())
        }
    }

    fn temp_index_with_chunk(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut store = Store::open(&path).unwrap();
        store
            .upsert_chunks(&[ChunkRecord {
                entry_path: "/doc.md".to_owned(),
                seq: 0,
                heading: String::new(),
                text: text.to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
            }])
            .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn answer_empty_hits_short_circuits_without_calling_llm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        Store::open(&path).unwrap(); // empty index, no chunks

        let gen_calls = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let llm = CountingGen {
            calls: gen_calls.clone(),
            reply: "should never be used".to_owned(),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            ..QaConfig::default()
        };

        let ans = answer(&path, &embedder, &llm, "anything", &cfg)
            .await
            .unwrap();
        assert!(ans.answer.contains("indexa deep"));
        assert!(ans.sources.is_empty());
        assert_eq!(
            gen_calls.load(Ordering::SeqCst),
            0,
            "empty hits must short-circuit before any LLM call"
        );
    }

    #[tokio::test]
    async fn answer_sparse_mode_skips_embedding() {
        let (_dir, path) = temp_index_with_chunk("rustacean ferris crab content");
        let embed_calls = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: embed_calls.clone(),
        };
        let llm = CountingGen {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: "answer".to_owned(),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            ..QaConfig::default()
        };

        let ans = answer(&path, &embedder, &llm, "ferris", &cfg)
            .await
            .unwrap();
        assert_eq!(
            embed_calls.load(Ordering::SeqCst),
            0,
            "Sparse mode must not embed the query"
        );
        assert_eq!(ans.answer, "answer");
    }

    #[tokio::test]
    async fn answer_synthesizes_from_hits() {
        let (_dir, path) = temp_index_with_chunk("the quick brown fox jumps over");
        let gen_calls = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let llm = CountingGen {
            calls: gen_calls.clone(),
            reply: "a synthesized answer".to_owned(),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            ..QaConfig::default()
        };

        let ans = answer(&path, &embedder, &llm, "fox", &cfg).await.unwrap();
        assert_eq!(ans.answer, "a synthesized answer");
        assert!(!ans.sources.is_empty());
        assert_eq!(gen_calls.load(Ordering::SeqCst), 1);
    }
}
