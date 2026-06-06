use std::path::Path;

use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{AnnIndex, SearchHit, Store};
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

#[derive(Debug, Clone)]
pub struct SourceCitation {
    pub path: String,
    pub heading: String,
    pub snippet: String,
}

/// An event emitted by [`answer_stream`]: the cited sources once up front (so a UI can
/// render citations before any token arrives), then answer text fragments as the model
/// produces them. Providers without real token streaming (everything but Ollama today)
/// emit a single `Fragment` with the whole answer.
pub enum AnswerChunk {
    Sources(Vec<SourceCitation>),
    Fragment(String),
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
    /// Apply importance weights (v0.8) as a multiplicative boost after RRF fusion.
    pub use_weights: bool,
    /// Max retrieval hops for the agentic ([`answer_agentic`]) path. Clamped to
    /// `1..=AGENTIC_MAX_STEPS_CAP`. Ignored by the one-shot [`answer`].
    pub max_steps: usize,
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
            use_weights: true,
            max_steps: 3,
        }
    }
}

/// Synchronous retrieval: hybrid search + summary boost. Kept separate so the
/// async orchestrator ([`answer`]) can scope the `&Store` borrow to a block that
/// never spans an `.await` — keeping the resulting future `Send` (required by the
/// axum web server and the rmcp MCP server). `query_vec` is `None` for sparse-only.
pub(crate) fn retrieve(
    store: &Store,
    question: &str,
    query_vec: Option<&[f32]>,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<Vec<SearchHit>> {
    let mut hits = store.hybrid_search_with_ann(
        question,
        query_vec,
        &cfg.mode,
        cfg.scope.as_deref(),
        cfg.top_k,
        cfg.rrf_k,
        ann,
    )?;
    if let Some(qvec) = query_vec {
        let _ = store.boost_with_summaries(
            &mut hits,
            qvec,
            cfg.summary_weight,
            cfg.summary_depth_alpha,
        );
    }
    // v0.8: apply per-file/dir/category importance weight boosts (multiplicative).
    if cfg.use_weights && !hits.is_empty() {
        let _ = store.boost_with_weights(&mut hits);
        // Re-sort after weight boost.
        hits.sort_by(|a, b| {
            b.rrf_score
                .partial_cmp(&a.rrf_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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
    answer_with_ann(db_path, embedder, llm, question, cfg, None).await
}

/// [`answer`] with an optional ANN index for dense retrieval (see
/// [`Store::hybrid_search_with_ann`](indexa_core::store::Store::hybrid_search_with_ann)).
/// Long-lived callers (the web server) build + cache the index and pass it here; one-shot
/// callers pass `None` and get brute-force. `None` ⇒ identical to [`answer`].
pub async fn answer_with_ann(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<Answer> {
    let hits = retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann).await?;
    if hits.is_empty() {
        return Ok(no_match_answer(question));
    }
    // Synthesize (no store access).
    synthesize_from_hits(hits, llm, question, cfg).await
}

/// Hard cap on agentic retrieval hops, regardless of `cfg.max_steps`. Each hop is
/// one retrieval + (except the last) one "decide" LLM call, so this bounds latency.
pub const AGENTIC_MAX_STEPS_CAP: usize = 5;
/// How many of the pooled hits to show the model in the gap-analysis digest.
const AGENTIC_DIGEST_HITS: usize = 10;

/// Agentic multi-step Q&A: a bounded *iterative retrieval* ("self-ask") loop.
/// Search → ask the model whether an important part of the question is still
/// uncovered and, if so, for one focused follow-up query → search again →
/// synthesize a cited answer from the merged context. This finds material a single
/// query misses (compositional questions, scattered context) at the cost of a few
/// extra LLM calls — hence opt-in (`indexa ask --agentic`, MCP `agentic: true`);
/// the default [`answer`] stays one-shot.
///
/// **Fails open by design.** The model's between-hop decision is parsed leniently;
/// an unparseable reply, a repeated query, or a hop that surfaces no new chunks all
/// end the loop. A model that won't emit `SEARCH:`/`DONE` therefore degrades to a
/// single retrieval rather than erroring or looping forever.
///
/// `on_step(step, query)` is called once per hop (1-based) so a surface can show
/// progress (the CLI prints it; a no-op closure is fine).
pub async fn answer_agentic(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    on_step: &mut (dyn FnMut(usize, &str) + Send),
) -> Result<Answer> {
    let hits = agentic_retrieve(db_path, embedder, llm, question, cfg, None, on_step).await?;
    if hits.is_empty() {
        return Ok(no_match_answer(question));
    }
    synthesize_from_hits(hits, llm, question, cfg).await
}

/// The agentic hop loop: returns the merged, deduplicated, re-ranked hit pool.
/// Each hop reuses [`retrieve`], so `cfg.scope` and the summary/importance boosts
/// apply on every hop (a follow-up never leaks outside the requested scope). The
/// `&Store` is opened and dropped inside a sync block each hop so the future stays
/// `Send` (required by the MCP/web servers).
async fn agentic_retrieve(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    on_step: &mut (dyn FnMut(usize, &str) + Send),
) -> Result<Vec<SearchHit>> {
    let max_steps = cfg.max_steps.clamp(1, AGENTIC_MAX_STEPS_CAP);
    let mut pool: Vec<SearchHit> = Vec::new();
    let mut seen_chunks: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut seen_queries: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut current = question.to_owned();

    for step in 0..max_steps {
        on_step(step + 1, &current);
        seen_queries.insert(normalize_query(&current));

        // Embed outside the Store scope (await), then retrieve in a sync block that
        // drops the connection before the next await.
        let query_vec = match cfg.mode {
            HybridMode::Sparse => None,
            _ => Some(embedder.embed(&current).await?),
        };
        let hits = {
            let store = Store::open(db_path)?;
            retrieve(&store, &current, query_vec.as_deref(), cfg, ann)?
        };

        let mut added = 0usize;
        for h in hits {
            if seen_chunks.insert(h.chunk_id) {
                pool.push(h);
                added += 1;
            }
        }

        // Stop on the last allowed hop (a follow-up couldn't be used) or when a hop
        // adds nothing new (a reworded query hitting the same chunks).
        if step + 1 >= max_steps || added == 0 {
            break;
        }

        // Decide: is a key aspect still missing? Ask for one follow-up query or DONE.
        let digest = build_digest(&pool, AGENTIC_DIGEST_HITS);
        let decision = llm.generate(&decide_prompt(question, &digest)).await?;
        match parse_followup(&decision) {
            Some(q) if !seen_queries.contains(&normalize_query(&q)) => current = q,
            _ => break, // DONE / unparseable / repeated → synthesize with what we have
        }
    }

    // Hits came from several searches; re-rank the merged pool before synthesis.
    pool.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(pool)
}

fn normalize_query(q: &str) -> String {
    q.trim().to_lowercase()
}

/// A compact, gap-focused digest of the pool — `[n] path — heading` lines, not the
/// full packed context. The decide call must see what's *covered* (to spot gaps),
/// not enough to answer (which would always say DONE and waste a long generation).
fn build_digest(hits: &[SearchHit], max: usize) -> String {
    if hits.is_empty() {
        return "(nothing found yet)".to_owned();
    }
    hits.iter()
        .take(max)
        .enumerate()
        .map(|(i, h)| {
            if h.heading.is_empty() {
                format!("[{}] {}", i + 1, h.entry_path)
            } else {
                format!("[{}] {} — {}", i + 1, h.entry_path, h.heading)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn decide_prompt(question: &str, digest: &str) -> String {
    format!(
        "You are gathering context to answer a question by searching a file index.\n\
         \n\
         QUESTION: {question}\n\
         \n\
         Search has found these files/sections so far:\n\
         {digest}\n\
         \n\
         If an important part of the question is NOT yet covered and one more search \
         would help, reply with EXACTLY one line:\n\
         SEARCH: <a short, focused query for the missing part>\n\
         Otherwise, if the found context is enough to answer, reply with exactly:\n\
         DONE"
    )
}

/// Lenient parse of the decide reply — returns the follow-up query if the model
/// asked to search again, else `None` (DONE, or anything unrecognised → fail open).
/// Scans every line and tolerates markdown/bullet noise, so a chatty model that
/// prefixes reasoning before the action line still works.
fn parse_followup(reply: &str) -> Option<String> {
    for raw in reply.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['-', '*', '>', '#', '`', ' '])
            .trim();
        if let Some(rest) = strip_prefix_ci(line, "search:") {
            let q = rest.trim().trim_matches(['"', '*', '`', ' ']).trim();
            if !q.is_empty() {
                return Some(q.to_owned());
            }
        }
        // A bare "DONE" (possibly with trailing punctuation/markdown) ends the loop.
        let bare = line.trim_matches(['.', '*', '`', ' ']);
        if bare.eq_ignore_ascii_case("done") {
            return None;
        }
    }
    None
}

/// ASCII-case-insensitive prefix strip that never panics on a multibyte boundary.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Streaming variant of [`answer`]: identical retrieve → rerank → synthesize pipeline, but
/// emits [`AnswerChunk`]s via `on_chunk` — `Sources` first (citations are known before
/// generation), then answer `Fragment`s as the LLM streams them. Returns the same
/// [`Answer`] as [`answer`] so callers that also want the assembled result (logging, tests)
/// get it. The no-match short-circuit emits its guidance as a single fragment so streaming
/// and non-streaming surfaces read identically.
pub async fn answer_stream(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    on_chunk: &mut (dyn FnMut(AnswerChunk) + Send),
) -> Result<Answer> {
    answer_stream_with_ann(db_path, embedder, llm, question, cfg, None, on_chunk).await
}

/// [`answer_stream`] with an optional ANN index for dense retrieval. `None` ⇒ identical to
/// [`answer_stream`].
#[allow(clippy::too_many_arguments)]
pub async fn answer_stream_with_ann(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    on_chunk: &mut (dyn FnMut(AnswerChunk) + Send),
) -> Result<Answer> {
    let hits = retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann).await?;
    if hits.is_empty() {
        let ans = no_match_answer(question);
        on_chunk(AnswerChunk::Sources(Vec::new()));
        on_chunk(AnswerChunk::Fragment(ans.answer.clone()));
        return Ok(ans);
    }

    let (context, sources) = pack_context(&hits, cfg.context_budget);
    // Citations up front so the UI can render them before the first token.
    on_chunk(AnswerChunk::Sources(sources.clone()));

    let prompt = build_prompt(question, &context);
    let mut full = String::new();
    {
        let mut on_frag = |frag: String| {
            full.push_str(&frag);
            on_chunk(AnswerChunk::Fragment(frag));
        };
        llm.generate_stream(&prompt, &mut on_frag).await?;
    }
    Ok(Answer {
        question: question.to_owned(),
        answer: full.trim().to_owned(),
        sources,
    })
}

/// Embed → retrieve → optional rerank, shared by [`answer`] and [`answer_stream`].
/// Returns an empty `Vec` when nothing matched (callers emit the no-match guidance).
/// The `&Store` is confined to a sync scope and dropped before any `.await`, so the
/// returned future is `Send`.
async fn retrieve_and_rerank(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<Vec<SearchHit>> {
    // 1. Embed (no store in scope). Skip for sparse-only.
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(question).await?),
    };

    // 2. Retrieve in a sync scope — `&Store` never crosses an await.
    let hits = {
        let store = Store::open(db_path)?;
        retrieve(&store, question, query_vec.as_deref(), cfg, ann)?
    };

    // 2b. No matches: with zero grounding the LLM would hallucinate a confident answer, so
    //     callers short-circuit to a "run indexa deep first" message (do not rerank).
    if hits.is_empty() {
        return Ok(Vec::new());
    }

    // 3. Optional cross-encoder rerank (fails open). Reaches every surface
    //    because they all call this helper.
    let hits = if cfg.rerank {
        apply_rerank(&LlmReranker::new(llm), question, hits).await
    } else {
        hits
    };
    Ok(hits)
}

/// The shared no-match answer (identical across CLI, web, MCP).
fn no_match_answer(question: &str) -> Answer {
    Answer {
        question: question.to_owned(),
        answer: "No indexed content matched your query. Run `indexa deep` and \
                 `indexa summarize` on the relevant folder first, then ask again."
            .to_owned(),
        sources: Vec::new(),
    }
}

/// Synthesise an answer from pre-retrieved hits (no store access). Internal helper for
/// [`answer`]; kept separate so the `&Store` borrow in `answer` never crosses an `.await`.
pub(crate) async fn synthesize_from_hits(
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

    /// Generator that streams several fragments (overrides generate_stream) so we can verify
    /// answer_stream preserves fragment order and event ordering.
    struct StreamingGen;
    #[async_trait::async_trait]
    impl Generator for StreamingGen {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            Ok("unused".to_owned())
        }
        async fn generate_stream(
            &self,
            _prompt: &str,
            on_fragment: &mut (dyn FnMut(String) + Send),
        ) -> Result<String> {
            let mut full = String::new();
            for part in ["Ferris ", "is the ", "Rust mascot."] {
                on_fragment(part.to_owned());
                full.push_str(part);
            }
            Ok(full)
        }
    }

    #[tokio::test]
    async fn answer_stream_emits_sources_before_fragments_in_order() {
        let (_dir, path) = temp_index_with_chunk("ferris the crab is the rust mascot");
        let embedder = CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            ..QaConfig::default()
        };

        let mut frags = String::new();
        let mut seen_fragment = false;
        let mut sources_before_fragment = true;
        let mut sources_count = None;
        {
            let mut on_chunk = |c: AnswerChunk| match c {
                AnswerChunk::Sources(s) => {
                    if seen_fragment {
                        sources_before_fragment = false;
                    }
                    sources_count = Some(s.len());
                }
                AnswerChunk::Fragment(t) => {
                    seen_fragment = true;
                    frags.push_str(&t);
                }
            };
            let ans = answer_stream(
                &path,
                &embedder,
                &StreamingGen,
                "ferris",
                &cfg,
                &mut on_chunk,
            )
            .await
            .unwrap();
            // Reaching the streamed text (not the no-match message) proves hits matched.
            assert_eq!(ans.answer, "Ferris is the Rust mascot.");
            assert_eq!(ans.sources.len(), 1);
        }
        assert!(
            sources_before_fragment,
            "Sources must be emitted before any fragment"
        );
        assert_eq!(sources_count, Some(1), "one source emitted up front");
        assert_eq!(
            frags, "Ferris is the Rust mascot.",
            "fragments must arrive in order and concatenate to the full answer"
        );
    }

    #[tokio::test]
    async fn answer_stream_no_match_emits_guidance_as_one_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        Store::open(&path).unwrap(); // empty index
        let embedder = CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            ..QaConfig::default()
        };
        let mut frags = String::new();
        let mut sources_len = None;
        {
            let mut on_chunk = |c: AnswerChunk| match c {
                AnswerChunk::Sources(s) => sources_len = Some(s.len()),
                AnswerChunk::Fragment(t) => frags.push_str(&t),
            };
            let ans = answer_stream(
                &path,
                &embedder,
                &StreamingGen,
                "anything",
                &cfg,
                &mut on_chunk,
            )
            .await
            .unwrap();
            assert!(ans.answer.contains("indexa deep"));
        }
        assert_eq!(sources_len, Some(0), "empty sources event still emitted");
        assert!(
            frags.contains("indexa deep"),
            "no-match guidance arrives as a fragment"
        );
    }

    // ── Agentic ask ───────────────────────────────────────────────────────────

    /// Generator that returns scripted replies in order (so an agentic-loop test can
    /// drive distinct decide/synthesis responses); falls back to "DONE" if exhausted.
    struct ScriptedGen {
        replies: std::sync::Mutex<std::collections::VecDeque<String>>,
        calls: Arc<AtomicUsize>,
    }
    impl ScriptedGen {
        fn new(replies: &[&str], calls: Arc<AtomicUsize>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
                calls,
            }
        }
    }
    #[async_trait::async_trait]
    impl Generator for ScriptedGen {
        async fn generate(&self, _prompt: &str) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "DONE".to_owned()))
        }
    }

    fn temp_index_with_chunks(chunks: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let mut store = Store::open(&path).unwrap();
        let records: Vec<ChunkRecord> = chunks
            .iter()
            .map(|(p, text)| ChunkRecord {
                entry_path: (*p).to_owned(),
                seq: 0,
                heading: String::new(),
                text: (*text).to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
            })
            .collect();
        store.upsert_chunks(&records).unwrap();
        (dir, path)
    }

    #[test]
    fn parse_followup_extracts_search_query() {
        assert_eq!(
            parse_followup("SEARCH: error handling").as_deref(),
            Some("error handling")
        );
        assert_eq!(
            parse_followup("search: lowercase ok").as_deref(),
            Some("lowercase ok")
        );
        // Tolerates leading reasoning + markdown noise around the action line.
        assert_eq!(
            parse_followup("Hmm, the auth part is missing.\n**SEARCH:** token refresh").as_deref(),
            Some("token refresh")
        );
    }

    #[test]
    fn parse_followup_done_and_garbage_stop_the_loop() {
        assert_eq!(parse_followup("DONE"), None);
        assert_eq!(parse_followup("I think we have enough.\nDONE."), None);
        assert_eq!(
            parse_followup("SEARCH:"),
            None,
            "empty query is not a follow-up"
        );
        assert_eq!(
            parse_followup("I'm not sure what you mean"),
            None,
            "unparseable reply fails open (stops the loop)"
        );
    }

    #[tokio::test]
    async fn agentic_runs_a_second_hop_and_merges_context() {
        // Two chunks matched by different BM25 terms; the follow-up surfaces the
        // second so the final answer draws on both hops.
        let (_d, path) = temp_index_with_chunks(&[
            ("/a.md", "alpha subsystem overview and design"),
            ("/b.md", "beta subsystem error handling details"),
        ]);
        let gen_calls = Arc::new(AtomicUsize::new(0));
        // Single-word follow-up ("beta") so it matches chunk B regardless of whether
        // the BM25 layer treats a multi-word query as a phrase or an AND.
        let llm = ScriptedGen::new(
            &["SEARCH: beta", "DONE", "Both covered [1][2]."],
            gen_calls.clone(),
        );
        let embedder = CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            max_steps: 3,
            ..QaConfig::default()
        };

        let mut steps: Vec<String> = Vec::new();
        let ans = answer_agentic(&path, &embedder, &llm, "alpha", &cfg, &mut |_i, q| {
            steps.push(q.to_owned())
        })
        .await
        .unwrap();

        assert_eq!(steps, vec!["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(ans.answer, "Both covered [1][2].");
        assert_eq!(
            ans.sources.len(),
            2,
            "both hops' chunks merged into the pool"
        );
        // decide#1 + decide#2 + synthesis = 3 generations.
        assert_eq!(gen_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn agentic_fails_open_to_single_hop_on_unparseable_decision() {
        let (_d, path) = temp_index_with_chunks(&[("/a.md", "alpha subsystem overview")]);
        let gen_calls = Arc::new(AtomicUsize::new(0));
        // Garbage decide reply ⇒ stop after one hop, then synthesize.
        let llm = ScriptedGen::new(&["uhh not sure", "Answer [1]."], gen_calls.clone());
        let embedder = CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            max_steps: 3,
            ..QaConfig::default()
        };

        let mut hops = 0usize;
        let ans = answer_agentic(&path, &embedder, &llm, "alpha", &cfg, &mut |_i, _q| {
            hops += 1
        })
        .await
        .unwrap();

        assert_eq!(
            hops, 1,
            "unparseable decision degrades to a single retrieval"
        );
        assert_eq!(ans.answer, "Answer [1].");
        assert_eq!(
            gen_calls.load(Ordering::SeqCst),
            2,
            "one decide call + one synthesis"
        );
    }
}
