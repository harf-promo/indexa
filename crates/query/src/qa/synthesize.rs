//! Answer synthesis: the public entry points ([`answer`], [`answer_stream`] and their
//! `_with_ann` variants), the shared embed→retrieve→rerank helper, context packing,
//! prompt construction, and the continuation trim. No `&Store` crosses an `.await`.

use std::path::Path;

use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{AnnIndex, SearchHit, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;

use crate::rerank::{apply_rerank, CandleReranker, LlmReranker};

use super::confidence::confidence_for;
use super::retrieve::{build_project_overview, is_broad_intent, retrieve};
use super::{Answer, AnswerChunk, QaConfig, SourceCitation};

/// Run the full RAG Q&A pipeline against the index at `db_path`:
///   embed(query) → retrieve → \[rerank\] → synthesize → cited answer.
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
    let (hits, overview) = retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann).await?;
    if hits.is_empty() {
        return Ok(no_match_answer(question));
    }
    // Synthesize (no store access).
    synthesize_from_hits(hits, overview, llm, question, cfg).await
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
    let (hits, overview) = retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann).await?;
    if hits.is_empty() {
        let ans = no_match_answer(question);
        on_chunk(AnswerChunk::Sources(Vec::new()));
        on_chunk(AnswerChunk::Fragment(ans.answer.clone()));
        return Ok(ans);
    }

    // Confidence is a property of the retrieval pool, fixed before synthesis.
    let confidence = confidence_for(&hits, cfg);
    let (context, sources) = pack_context(&hits, &overview, cfg.context_budget);
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
        confidence,
    })
}

/// Embed → retrieve → optional rerank + project overview, shared by [`answer`] and
/// [`answer_stream`]. Returns `(hits, overview)`. Empty hits ⇒ callers short-circuit.
/// The `&Store` is confined to a sync scope and dropped before any `.await`, so the
/// returned future is `Send`. The `overview` is a pre-budgeted PROJECT OVERVIEW string
/// (empty when no dir summaries exist or for specific questions).
async fn retrieve_and_rerank(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<(Vec<SearchHit>, String)> {
    // 1. Embed (no store in scope). Skip for sparse-only.
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(question).await?),
    };

    // 2. Retrieve + build project overview in a sync scope — `&Store` never crosses an await.
    let (hits, overview) = {
        let store = Store::open(db_path)?;
        let hits = retrieve(&store, question, query_vec.as_deref(), cfg, ann)?;
        // Compute project-overview block while the store is still open.
        // Budget: broad questions get ~35% of context_budget (≤1400); specific → 300 chars
        // for just the root one-liner. Always subtracted FROM the chunk budget, never added.
        let overview_budget = if is_broad_intent(question) {
            cfg.context_budget * 35 / 100
        } else {
            300
        };
        let overview = build_project_overview(&store, &hits, cfg.scope.as_deref(), overview_budget);
        (hits, overview)
    };

    // 2b. No matches: with zero grounding the LLM would hallucinate a confident answer, so
    //     callers short-circuit to a "run indexa deep first" message (do not rerank).
    if hits.is_empty() {
        return Ok((Vec::new(), String::new()));
    }

    // 3. Optional cross-encoder rerank (fails open). Reaches every surface
    //    because they all call this helper.
    let hits = if cfg.rerank {
        if cfg.rerank_backend == "cross-encoder" {
            apply_rerank(&CandleReranker::new(), question, hits).await
        } else {
            apply_rerank(&LlmReranker::new(llm), question, hits).await
        }
    } else {
        hits
    };
    Ok((hits, overview))
}

/// The shared no-match answer (identical across CLI, web, MCP). Deliberately carries
/// no confidence label — the message already says the index has nothing.
pub(crate) fn no_match_answer(question: &str) -> Answer {
    Answer {
        question: question.to_owned(),
        answer: "No indexed content matched your query. Run `indexa deep` and \
                 `indexa summarize` on the relevant folder first, then ask again."
            .to_owned(),
        sources: Vec::new(),
        confidence: None,
    }
}

/// Synthesise an answer from pre-retrieved hits (no store access). Internal helper for
/// [`answer`]; kept separate so the `&Store` borrow in `answer` never crosses an `.await`.
/// `overview` is the pre-budgeted PROJECT OVERVIEW string (may be empty).
pub(crate) async fn synthesize_from_hits(
    hits: Vec<SearchHit>,
    overview: String,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
) -> Result<Answer> {
    // Confidence is a property of the retrieval pool, fixed before synthesis
    // (rerank only reorders; assess_confidence sorts scores internally).
    let confidence = confidence_for(&hits, cfg);
    let (context, sources) = pack_context(&hits, &overview, cfg.context_budget);
    let prompt = build_prompt(question, &context);
    let answer_text = llm.generate(&prompt).await?;
    Ok(Answer {
        question: question.to_owned(),
        // Cut any hallucinated transcript continuation (see `trim_continuation`).
        answer: trim_continuation(&answer_text),
        sources,
        confidence,
    })
}

/// Keep only the answer to the asked question. The `QUESTION:/ANSWER:` prompt frame can lead an
/// instruct/base model to keep going with an invented next turn (observed live: it appended
/// "QUESTION: what should you do when contributing? ANSWER: …"). Cut at the first such marker so
/// the user never sees a fabricated extra Q&A. Defensive — the prompt also forbids it.
pub(crate) fn trim_continuation(text: &str) -> String {
    let mut end = text.len();
    for marker in [
        "\nQUESTION:",
        "\nQuestion:",
        "\nQ:",
        "\nANSWER:",
        "\n\nQUESTION",
    ] {
        if let Some(i) = text.find(marker) {
            end = end.min(i);
        }
    }
    text[..end].trim().to_owned()
}

pub(crate) fn pack_context(
    hits: &[SearchHit],
    overview: &str,
    budget: usize,
) -> (String, Vec<SourceCitation>) {
    let mut context = String::new();
    let mut sources = Vec::new();

    // Prepend the project-overview block (already budget-bounded by build_project_overview).
    // It is NOT cited (no citation numbers) — it's background context, not a retrievable chunk.
    if !overview.is_empty() {
        context.push_str(overview);
        if !overview.ends_with('\n') {
            context.push('\n');
        }
        context.push('\n');
    }

    // Remaining budget for chunk citations.
    let mut chars_used = context.len();

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
                // Signal to the synthesizer that this chunk was cut to fit the budget, so it
                // doesn't treat the partial text as the whole file.
                context.push_str("\n…[chunk truncated to fit the context budget]\n\n");
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

pub(crate) fn build_prompt(question: &str, context: &str) -> String {
    format!(
        "You are a helpful assistant that answers questions about a user's files.\n\
         The context below may begin with a PROJECT OVERVIEW block: directory roll-up summaries \
         describing the project as a whole. Use it to give a coherent, project-level answer when \
         the question is broad (e.g. \"what is this project about\", \"main themes\"). For specific \
         claims, cite the numbered excerpts by their [number]; the overview itself is background and \
         is not numbered.\n\
         Use ONLY the provided context to answer. Cite sources by their [number].\n\
         If the answer isn't in the context, say so.\n\
         Answer ONLY the question below. Do not invent or answer any other question, and do not \
         continue with another \"QUESTION:\" line — stop when the answer is complete.\n\
         Some context may be historical or archived (paths containing /archive/, or an old version \
         marker like v0.2.2). Prefer current sources and never present an outdated fact as current.\n\
         \n\
         CONTEXT:\n\
         {context}\n\
         \n\
         QUESTION: {question}\n\
         \n\
         ANSWER:"
    )
}
