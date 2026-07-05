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

use super::cluster::{cluster_hits, cluster_theme_prompt, Cluster};
use super::confidence::confidence_for;
use super::retrieve::{build_project_overview, is_broad_intent, retrieve};
use super::rewrite::resolve_search_query;
use super::{Answer, AnswerChunk, PriorTurn, QaConfig, SourceCitation};

/// Per-cluster member text budget (chars) fed into the theme-summary prompt. Keeps the optional
/// `graphrag_summarize` LLM calls cheap and bounded regardless of how large the cluster's chunks are.
const CLUSTER_SUMMARY_INPUT_BUDGET: usize = 1200;

/// Fraction of the context budget a conversation-history block may consume before it
/// starts crowding out retrieved chunks. The block is trimmed oldest-first to fit.
const HISTORY_BUDGET_PCT: usize = 25;

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
    answer_with_ann_history(db_path, embedder, llm, question, cfg, ann, &[]).await
}

/// [`answer_with_ann`] with prior conversation turns (Conversational Ask). When `history`
/// is non-empty the follow-up is rewritten into a standalone search query before retrieval
/// (one extra LLM call, fail-open), and the turns are folded into the synthesis prompt
/// budget-clamped. `history = &[]` ⇒ byte-for-byte identical to [`answer_with_ann`].
#[allow(clippy::too_many_arguments)]
pub async fn answer_with_ann_history(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    history: &[PriorTurn],
) -> Result<Answer> {
    let (hits, overview, clusters) =
        retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann, history).await?;
    if hits.is_empty() {
        return Ok(no_match_answer(question));
    }
    // Synthesize (no store access). `clusters` is empty unless GraphRAG clustering applied.
    synthesize_from_hits_clustered(hits, overview, clusters, llm, question, cfg, history).await
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
    answer_stream_with_ann_history(db_path, embedder, llm, question, cfg, ann, &[], on_chunk).await
}

/// [`answer_stream_with_ann`] with prior conversation turns (Conversational Ask).
/// `history = &[]` ⇒ identical to [`answer_stream_with_ann`].
#[allow(clippy::too_many_arguments)]
pub async fn answer_stream_with_ann_history(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    history: &[PriorTurn],
    on_chunk: &mut (dyn FnMut(AnswerChunk) + Send),
) -> Result<Answer> {
    let (hits, overview, mut clusters) =
        retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann, history).await?;
    if hits.is_empty() {
        let ans = no_match_answer(question);
        on_chunk(AnswerChunk::Sources(Vec::new()));
        on_chunk(AnswerChunk::Fragment(ans.answer.clone()));
        return Ok(ans);
    }

    // Optional per-cluster theme summaries (graphrag_summarize) — bounded, fail-open.
    maybe_summarize_clusters(&mut clusters, llm, cfg).await;

    // Confidence is a property of the retrieval pool, fixed before synthesis.
    let confidence = confidence_for(&hits, cfg, question);
    let (history_block, chunk_budget) = split_history_budget(history, cfg.context_budget);
    let clustered = !clusters.is_empty();
    let (context, sources) = pack_context_clustered(&hits, &overview, &clusters, chunk_budget);
    // Citations up front so the UI can render them before the first token.
    on_chunk(AnswerChunk::Sources(sources.clone()));

    let prompt = build_prompt_clustered(question, &context, &history_block, clustered);
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
        synthesized: true,
        model: None,
    })
}

/// Run the full retrieve → rerank pipeline and return the **packed context slice** WITHOUT
/// the final LLM synthesis. The returned [`Answer`] carries the retrieved slice in `answer`
/// (the exact pack the synthesizer would have seen: PROJECT OVERVIEW + numbered `[N]` chunks),
/// the matching `sources`, the retrieval-coverage `confidence`, and `synthesized = false`.
///
/// This exposes Indexa's full retrieval intelligence (hybrid, boosts, rerank, MMR, per-file cap,
/// overview, coverage) as a context provider, so a **stronger caller** (e.g. a cloud model over
/// MCP) can synthesize the answer with its own model instead of paying for — and being capped
/// by — the local generation model. `history = &[]` ⇒ no follow-up rewrite.
pub async fn answer_retrieval_only(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<Answer> {
    answer_retrieval_only_history(db_path, embedder, llm, question, cfg, ann, &[]).await
}

/// [`answer_retrieval_only`] with prior conversation turns (Conversational Ask): the follow-up
/// is rewritten into a standalone search query before retrieval (history-gated, fail-open).
/// `history = &[]` ⇒ identical to [`answer_retrieval_only`].
#[allow(clippy::too_many_arguments)]
pub async fn answer_retrieval_only_history(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    history: &[PriorTurn],
) -> Result<Answer> {
    let (hits, overview, mut clusters) =
        retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann, history).await?;
    if hits.is_empty() {
        // No grounding to return — reuse the canned guidance, flagged as not synthesized so the
        // caller doesn't mistake it for a context slice.
        let mut a = no_match_answer(question);
        a.synthesized = false;
        return Ok(a);
    }
    // The slice should match what a synthesizer would see, including GraphRAG theme grouping.
    maybe_summarize_clusters(&mut clusters, llm, cfg).await;
    let confidence = confidence_for(&hits, cfg, question);
    // Pack the slice exactly as the synthesizer would (overview + numbered chunks). The
    // conversation-history block is deliberately omitted: a self-synthesizing caller has its
    // own history; what they want from Indexa is the retrieved evidence.
    let (context, sources) =
        pack_context_clustered(&hits, &overview, &clusters, cfg.context_budget);
    Ok(Answer {
        question: question.to_owned(),
        answer: context,
        sources,
        confidence,
        synthesized: false,
        model: None,
    })
}

/// **Catalog (progressive-disclosure) retrieval** — returns a scored list of files without
/// synthesizing an answer. Each entry shows the file path, its L0 one-line abstract, and its
/// RRF retrieval score.
///
/// Use this when the caller is a capable LLM that wants to *choose* which files to expand
/// (via `get_summary`, `read_file`, or `get_chunk_context`) rather than receiving a single
/// synthesized answer. This is the "table of contents" step in a progressive-disclosure loop:
///
/// ```text
/// ask(catalog:true) → pick interesting paths → get_summary / read_file → synthesize yourself
/// ```
///
/// Callers get bounded KV-cache: only the L0 abstracts (≤1 sentence each) are sent, not
/// the full chunk bodies. The full retrieval pipeline still runs (hybrid + boosts + rerank +
/// MMR + per-file cap), but results are deduplicated to the file level before returning.
pub async fn answer_catalog(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<Answer> {
    answer_catalog_history(db_path, embedder, llm, question, cfg, ann, &[]).await
}

/// [`answer_catalog`] with prior conversation turns for follow-up rewriting.
/// `history = &[]` ⇒ identical to [`answer_catalog`].
#[allow(clippy::too_many_arguments)]
pub async fn answer_catalog_history(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    history: &[PriorTurn],
) -> Result<Answer> {
    let (hits, _overview, _clusters) =
        retrieve_and_rerank(db_path, embedder, llm, question, cfg, ann, history).await?;

    if hits.is_empty() {
        let mut a = no_match_answer(question);
        a.synthesized = false;
        return Ok(a);
    }

    // Deduplicate to file level: keep the best RRF score per path.
    let mut seen: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for hit in &hits {
        seen.entry(hit.entry_path.clone())
            .and_modify(|s| {
                if hit.rrf_score > *s {
                    *s = hit.rrf_score;
                }
            })
            .or_insert(hit.rrf_score);
    }
    // Sort by score descending, stable.
    let mut file_hits: Vec<(String, f64)> = seen.into_iter().collect();
    file_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Fetch L0 abstracts in a sync scope (no &Store crossing .await).
    let lines: Vec<String> = {
        let store = Store::open(db_path)?;
        file_hits
            .iter()
            .map(|(path, score)| {
                let l0 = store
                    .summary_by_path(path)
                    .ok()
                    .flatten()
                    .and_then(|s| s.summary_l0)
                    .unwrap_or_default();
                if l0.is_empty() {
                    format!("{path}  (score {score:.3})")
                } else {
                    format!("{path} — {l0}  (score {score:.3})")
                }
            })
            .collect()
    };

    let sources: Vec<SourceCitation> = file_hits
        .iter()
        .map(|(path, _)| SourceCitation {
            path: path.clone(),
            heading: String::new(),
            snippet: String::new(),
        })
        .collect();

    let body = format!(
        "CATALOG — {n} files matching \"{question}\".\n\
         Expand a file with `get_summary`, `read_file`, or `get_chunk_context`.\n\n\
         {list}",
        n = lines.len(),
        list = lines.join("\n")
    );

    Ok(Answer {
        question: question.to_owned(),
        answer: body,
        sources,
        confidence: None,
        synthesized: false,
        model: None,
    })
}

/// Embed → retrieve → optional rerank + project overview, shared by [`answer`] and
/// [`answer_stream`]. Returns `(hits, overview, clusters)`. Empty hits ⇒ callers short-circuit.
/// The `&Store` is confined to a sync scope and dropped before any `.await`, so the
/// returned future is `Send`. The `overview` is a pre-budgeted PROJECT OVERVIEW string
/// (empty when no dir summaries exist or for specific questions). `clusters` is empty unless
/// GraphRAG clustering applied (broad, unscoped question + `graphrag_clusters`); when non-empty
/// it is a regrouping (permutation) of `hits` used by the clustered packing.
#[allow(clippy::too_many_arguments)]
async fn retrieve_and_rerank(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    history: &[PriorTurn],
) -> Result<(Vec<SearchHit>, String, Vec<Cluster>)> {
    // 0. Conversational: resolve the follow-up into a standalone search query (history-gated,
    //    one extra LLM call, fail-open). The ORIGINAL question still drives the overview and
    //    the synthesis prompt — only retrieval (embed + FTS) sees the rewritten query.
    let search_query = resolve_search_query(llm, question, history).await;

    // 1. Embed (no store in scope). Skip for sparse-only.
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(&search_query).await?),
    };

    // GraphRAG clustering is gated like the per-file cap: only broad, unscoped questions, and only
    // when enabled. Focused/scoped asks are byte-identical to today (clusters stays empty).
    let want_clusters = cfg.graphrag_clusters && cfg.scope.is_none() && is_broad_intent(question);

    // 2. Retrieve + build project overview in a sync scope — `&Store` never crosses an await.
    //    Also fetch the chunk embeddings here (same open connection) when clustering wants them;
    //    rerank only reorders hits, never changes chunk_ids, so the map stays valid afterward.
    let (hits, overview, emb_map) = {
        let store = Store::open(db_path)?;
        let hits = retrieve(&store, &search_query, query_vec.as_deref(), cfg, ann)?;
        // Compute project-overview block while the store is still open.
        // Budget: broad questions get ~35% of context_budget (≤1400); specific → 300 chars
        // for just the root one-liner. Always subtracted FROM the chunk budget, never added.
        let overview_budget = if is_broad_intent(question) {
            cfg.context_budget * 35 / 100
        } else {
            300
        };
        let overview = build_project_overview(&store, &hits, cfg.scope.as_deref(), overview_budget);
        let emb_map = if want_clusters && hits.len() >= 2 {
            let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
            store.embeddings_for_chunks(&ids).unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };
        (hits, overview, emb_map)
    };

    // 2b. No matches: with zero grounding the LLM would hallucinate a confident answer, so
    //     callers short-circuit to a "run indexa deep first" message (do not rerank).
    if hits.is_empty() {
        return Ok((Vec::new(), String::new(), Vec::new()));
    }

    // 3. Optional cross-encoder rerank (fails open). Reaches every surface
    //    because they all call this helper.
    let hits = if cfg.rerank {
        if cfg.rerank_backend == "cross-encoder" {
            apply_rerank(&CandleReranker::new(&cfg.rerank_model), question, hits).await
        } else {
            apply_rerank(&LlmReranker::new(llm), question, hits).await
        }
    } else {
        hits
    };

    // 4. GraphRAG clustering (post-rerank, reusing the pre-fetched embeddings). Empty unless
    //    enabled — fails open to a single cluster inside `cluster_hits`, which the packer renders
    //    identically to the flat path. We keep the flat `hits` too (for confidence + the slice).
    let clusters = if !emb_map.is_empty() {
        cluster_hits(
            hits.clone(),
            &emb_map,
            cfg.graphrag_cluster_sim,
            cfg.graphrag_max_clusters,
        )
    } else {
        Vec::new()
    };
    Ok((hits, overview, clusters))
}

/// Optionally fill each multi-member cluster's `summary` with a one-line theme via a bounded local
/// LLM call (`graphrag_summarize`). No-op when summarization is off, there are no real clusters
/// (≤1), or a cluster has a single member. **Fail-open**: a failed/empty summary just stays `None`
/// (the cluster still renders, without a theme line).
async fn maybe_summarize_clusters(clusters: &mut [Cluster], llm: &dyn Generator, cfg: &QaConfig) {
    if !cfg.graphrag_summarize || clusters.len() < 2 {
        return;
    }
    for cluster in clusters.iter_mut() {
        if cluster.members.len() < 2 {
            continue; // a lone chunk is its own theme; skip the call
        }
        let mut joined = String::new();
        for m in &cluster.members {
            if joined.len() >= CLUSTER_SUMMARY_INPUT_BUDGET {
                break;
            }
            let take = CLUSTER_SUMMARY_INPUT_BUDGET - joined.len();
            joined.push_str(&m.text.chars().take(take).collect::<String>());
            joined.push('\n');
        }
        if let Ok(theme) = llm.generate(&cluster_theme_prompt(&joined)).await {
            let theme = theme.trim();
            if !theme.is_empty() && theme.len() <= 120 {
                cluster.summary = Some(theme.to_owned());
            }
        }
    }
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
        synthesized: true,
        model: None,
    }
}

/// Synthesise an answer from pre-retrieved hits (no store access). Internal helper for
/// [`answer`]; kept separate so the `&Store` borrow in `answer` never crosses an `.await`.
/// `overview` is the pre-budgeted PROJECT OVERVIEW string (may be empty). Delegates with no
/// clusters → flat packing (used by the agentic path, which doesn't cluster).
pub(crate) async fn synthesize_from_hits(
    hits: Vec<SearchHit>,
    overview: String,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    history: &[PriorTurn],
) -> Result<Answer> {
    synthesize_from_hits_clustered(hits, overview, Vec::new(), llm, question, cfg, history).await
}

/// [`synthesize_from_hits`] with GraphRAG clusters. When `clusters` is empty (the default and the
/// off path) the packing + prompt are **byte-identical** to the flat path. When non-empty, the
/// context is topic-grouped (with per-cluster theme summaries if `graphrag_summarize` ran).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn synthesize_from_hits_clustered(
    hits: Vec<SearchHit>,
    overview: String,
    mut clusters: Vec<Cluster>,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    history: &[PriorTurn],
) -> Result<Answer> {
    // Optional per-cluster theme summaries (graphrag_summarize) — bounded, fail-open, no-op when off.
    maybe_summarize_clusters(&mut clusters, llm, cfg).await;
    // Confidence is a property of the retrieval pool, fixed before synthesis
    // (rerank only reorders; assess_confidence sorts scores internally).
    let confidence = confidence_for(&hits, cfg, question);
    let (history_block, chunk_budget) = split_history_budget(history, cfg.context_budget);
    let clustered = !clusters.is_empty();
    let (context, sources) = pack_context_clustered(&hits, &overview, &clusters, chunk_budget);
    let prompt = build_prompt_clustered(question, &context, &history_block, clustered);
    let answer_text = llm.generate(&prompt).await?;
    Ok(Answer {
        question: question.to_owned(),
        // Cut any hallucinated transcript continuation (see `trim_continuation`).
        answer: trim_continuation(&answer_text),
        sources,
        confidence,
        synthesized: true,
        model: None,
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
    // Flat packing (today's behavior) = the clustered packer with no clusters.
    pack_context_clustered(hits, overview, &[], budget)
}

/// Emit one hit as a numbered `[n]` chunk into `context`/`sources`, honoring the char `budget`.
/// `*n` is the 1-based citation counter (incremented here). Returns `false` when the budget was
/// reached (the caller must stop emitting). Mirrors the original flat loop exactly so the
/// no-cluster path stays byte-identical: a partially-fitting chunk keeps its full `[n]` header and
/// gets a matching `SourceCitation` (no dangling citation); a chunk too small to fit even the
/// header is skipped without a citation.
fn push_hit(
    context: &mut String,
    sources: &mut Vec<SourceCitation>,
    chars_used: &mut usize,
    n: &mut usize,
    hit: &SearchHit,
    budget: usize,
) -> bool {
    *n += 1;
    let header = if hit.heading.is_empty() {
        format!("[{}] {}\n", *n, hit.entry_path)
    } else {
        format!("[{}] {} — {}\n", *n, hit.entry_path, hit.heading)
    };
    let chunk = format!("{}{}\n\n", header, hit.text);

    if *chars_used + chunk.len() > budget {
        let remaining = budget.saturating_sub(*chars_used);
        if remaining > header.len() + 40 {
            // Floor to a char boundary so we never slice mid-codepoint (a raw byte
            // offset panics on multibyte content: accents, CJK, emoji, em-dashes).
            let safe_end = indexa_core::text::floor_char_boundary(&chunk, remaining);
            context.push_str(&chunk[..safe_end]);
            // Signal to the synthesizer that this chunk was cut to fit the budget, so it
            // doesn't treat the partial text as the whole file.
            context.push_str("\n…[chunk truncated to fit the context budget]\n\n");
            // The truncated chunk keeps its full `[N]` header, so the model can still cite it —
            // push a matching SourceCitation so `sources` always covers the highest [N] in the
            // context and no citation dangles. Also keeps impact's served-bytes accounting honest.
            sources.push(SourceCitation {
                path: hit.entry_path.clone(),
                heading: hit.heading.clone(),
                snippet: hit.text.chars().take(120).collect::<String>() + "...",
            });
        }
        return false;
    }
    *chars_used += chunk.len();
    context.push_str(&chunk);
    sources.push(SourceCitation {
        path: hit.entry_path.clone(),
        heading: hit.heading.clone(),
        snippet: hit.text.chars().take(120).collect::<String>() + "...",
    });
    true
}

/// Pack retrieved context for the synthesis prompt. When `clusters` is empty this is **byte-for-byte
/// identical** to the legacy flat packing (overview block + numbered `[1..N]` chunks in `hits`
/// order). When non-empty (GraphRAG Approach C), the chunks are grouped under `=== THEME … ===`
/// headers (background, not cited) with a single GLOBAL `[1..N]` counter spanning all clusters, so
/// citations and `sources` stay 1:1 and the dangling-citation invariant holds across clusters.
pub(crate) fn pack_context_clustered(
    hits: &[SearchHit],
    overview: &str,
    clusters: &[Cluster],
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

    let mut chars_used = context.len();
    let mut n = 0usize; // global 1-based citation counter

    if clusters.is_empty() {
        // Flat path (byte-identical to the original).
        for hit in hits {
            if !push_hit(
                &mut context,
                &mut sources,
                &mut chars_used,
                &mut n,
                hit,
                budget,
            ) {
                break;
            }
        }
    } else {
        // Clustered path: a theme header (background) precedes each cluster's members.
        'outer: for (ci, cluster) in clusters.iter().enumerate() {
            let header = match &cluster.summary {
                Some(s) => format!("=== THEME: {s} ===\n"),
                None => format!("=== THEME {} ===\n", ci + 1),
            };
            // Stop if even the header can't fit (no room for any more content).
            if chars_used + header.len() + 40 > budget {
                break;
            }
            context.push_str(&header);
            chars_used += header.len();
            for hit in &cluster.members {
                if !push_hit(
                    &mut context,
                    &mut sources,
                    &mut chars_used,
                    &mut n,
                    hit,
                    budget,
                ) {
                    break 'outer;
                }
            }
        }
    }

    (context, sources)
}

/// Split the context budget between a conversation-history block and retrieved chunks.
/// Returns `(history_block, chunk_budget)`: the block is capped at `HISTORY_BUDGET_PCT`
/// of the budget (trimmed oldest-first to fit), and the chunk budget is what remains so
/// total prompt size stays bounded. Empty history ⇒ `("", full budget)`.
pub(crate) fn split_history_budget(history: &[PriorTurn], budget: usize) -> (String, usize) {
    if history.is_empty() {
        return (String::new(), budget);
    }
    let block = render_history_block(history, budget * HISTORY_BUDGET_PCT / 100);
    let remaining = budget.saturating_sub(block.len());
    (block, remaining)
}

/// Render prior turns as a `CONVERSATION SO FAR` block, oldest-first, dropping the
/// oldest turns until the whole block fits `budget` chars. A single over-long answer is
/// truncated at a char boundary. Returns `""` when nothing fits.
pub(crate) fn render_history_block(history: &[PriorTurn], budget: usize) -> String {
    if history.is_empty() || budget == 0 {
        return String::new();
    }
    const HEADER: &str =
        "CONVERSATION SO FAR (for reference; cite only the CONTEXT excerpts below):\n";
    // Render newest→oldest, keep as many recent turns as fit, then reverse to chronological.
    let mut kept: Vec<String> = Vec::new();
    let mut used = HEADER.len();
    for t in history.iter().rev() {
        let mut turn = format!("Q: {}\nA: {}\n", t.question.trim(), t.answer.trim());
        if used + turn.len() > budget {
            // Try to fit a truncated form of this (the oldest kept) turn, then stop.
            let remaining = budget.saturating_sub(used);
            if remaining > 80 {
                let safe = indexa_core::text::floor_char_boundary(&turn, remaining);
                turn.truncate(safe);
                turn.push_str("…\n");
                kept.push(turn);
            }
            break;
        }
        used += turn.len();
        kept.push(turn);
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.reverse();
    let mut block = String::from(HEADER);
    for t in kept {
        block.push_str(&t);
    }
    block
}

/// `history_block` is the pre-rendered, budget-clamped `CONVERSATION SO FAR` text (empty
/// for a single-shot Ask). It is inserted before CONTEXT and is background, not citable.
/// Delegates to [`build_prompt_clustered`] with `clustered = false` (the flat path).
pub(crate) fn build_prompt(question: &str, context: &str, history_block: &str) -> String {
    build_prompt_clustered(question, context, history_block, false)
}

/// [`build_prompt`] with an optional GraphRAG theme guidance line. When `clustered = false` the
/// output is **byte-identical** to the legacy prompt; when `true`, one extra sentence tells the
/// model the context is grouped into `=== THEME … ===` sections so it can structure a multi-faceted
/// answer (the theme lines are background — claims are still cited by `[number]`).
pub(crate) fn build_prompt_clustered(
    question: &str,
    context: &str,
    history_block: &str,
    clustered: bool,
) -> String {
    let convo = if history_block.is_empty() {
        String::new()
    } else {
        format!("{history_block}\n")
    };
    let theme_line = if clustered {
        "The CONTEXT is grouped into \"=== THEME … ===\" sections, each a cluster of related \
         excerpts (the theme line is background, not citable). Use the themes to structure a \
         coherent, multi-faceted answer, and still cite specific claims by their [number].\n"
    } else {
        ""
    };
    format!(
        "You are a helpful assistant that answers questions about a user's files.\n\
         The context below may begin with a PROJECT OVERVIEW block: directory roll-up summaries \
         describing the project as a whole. Use it to give a coherent, project-level answer when \
         the question is broad (e.g. \"what is this project about\", \"main themes\"). For specific \
         claims, cite the numbered excerpts by their [number]; the overview itself is background and \
         is not numbered.\n\
         {theme_line}\
         A CONVERSATION SO FAR block may also appear: it is the earlier turns of this chat, for \
         resolving references like \"it\"/\"that\". Treat it as background only — never cite it, and \
         answer the latest QUESTION.\n\
         Use ONLY the provided context to answer. Cite sources by their [number].\n\
         If the answer isn't in the context, say so.\n\
         Answer ONLY the question below. Do not invent or answer any other question, and do not \
         continue with another \"QUESTION:\" line — stop when the answer is complete.\n\
         When comparing several items, a short Markdown table is welcome; otherwise answer in prose.\n\
         Some context may be historical or archived (paths containing /archive/, or an old version \
         marker like v0.2.2). Prefer current sources and never present an outdated fact as current.\n\
         \n\
         {convo}\
         CONTEXT:\n\
         {context}\n\
         \n\
         QUESTION: {question}\n\
         \n\
         ANSWER:"
    )
}
