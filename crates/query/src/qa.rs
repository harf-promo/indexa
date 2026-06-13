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
    /// Retrieval-shape confidence (see [`assess_confidence`]). `None` only for the
    /// zero-hit short-circuit — that message already says the index has nothing,
    /// so bolting a confidence label onto it would be noise.
    pub confidence: Option<ConfidenceReport>,
}

/// Heuristic answer-level confidence. Derived purely from the *shape of the
/// retrieval pool* before synthesis — it says how well the index covered the
/// question, not whether the model's prose is correct. NOT calibrated: the
/// thresholds in [`assess_confidence`] are documented judgment calls, not
/// probabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ConfidenceReport {
    pub level: Confidence,
    /// One-line human explanation, e.g. "9 strong matches".
    pub basis: String,
    /// The raw numbers the level was derived from (`indexa ask --explain` prints them).
    pub inputs: ConfidenceInputs,
    /// Phase-2 placeholder: question aspects retrieval likely did not cover.
    /// Always `None` today.
    pub uncovered: Option<Vec<String>>,
}

/// The retrieval-shape numbers behind a [`ConfidenceReport`], surfaced by
/// `indexa ask --explain` so a user can see why a level was chosen.
#[derive(Debug, Clone)]
pub struct ConfidenceInputs {
    pub hit_count: usize,
    pub top_k: usize,
    pub top_score: f64,
    pub median_score: f64,
    /// top/median score ratio (≥ 1.0): large ⇒ one dominant hit, ~1 ⇒ a flat pool.
    pub gap: f64,
    /// Hits at or above `strong_floor`.
    pub strong_hits: usize,
    /// Fused-mass floor for a "strong" hit: `1/(rrf_k+10)` ≈ top-10 in one retriever.
    pub strong_floor: f64,
    /// Whether dense retrieval ran (a query embedding existed), i.e. corroboration
    /// between keyword and semantic rankings was possible at all.
    pub embeddings: bool,
}

/// Classify retrieval-pool shape into a [`ConfidenceReport`]. Pure and deterministic;
/// `None` only for an empty pool (the no-match short-circuit speaks for itself).
///
/// Anchors derive from the RRF formula — a hit at rank `r` in one retriever
/// contributes `1/(rrf_k + r)` fused mass — so thresholds track `rrf_k` and the
/// *relative* structure of the pool rather than absolute magic numbers:
/// - `rank1` = `1/(rrf_k+1)`: a clean rank-1 in a single retriever.
/// - strong hit: ≥ `1/(rrf_k+10)` (≈ top-10 in one retriever, or equivalent fused mass).
/// - corroborated top (hybrid only): ≥ 1.5 × `rank1`, reachable only when keyword and
///   semantic retrieval both rank the same chunk near their tops (an importance weight
///   can also push a hit there — accepted, it encodes user judgment).
///
/// Levels (heuristic, NOT calibrated):
/// - High: corroborated top + ≥ 3 strong hits + pool at least half of `top_k` + no
///   single hit dominating a weak remainder (gap ≤ 3).
/// - Low: no strong hits at all, or a single weak hit.
/// - Medium: everything in between.
pub fn assess_confidence(
    hits: &[SearchHit],
    top_k: usize,
    rrf_k: f32,
    embeddings: bool,
) -> Option<ConfidenceReport> {
    if hits.is_empty() {
        return None;
    }
    // Sort scores locally: callers may hand us reranked (reordered) hits, and the
    // shape metrics must not depend on presentation order.
    let mut scores: Vec<f64> = hits.iter().map(|h| h.rrf_score).collect();
    scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let n = scores.len();
    let top = scores[0];
    let median = scores[n / 2].max(f64::EPSILON);
    let gap = top / median;

    let rank1 = 1.0 / (rrf_k as f64 + 1.0);
    let strong_floor = 1.0 / (rrf_k as f64 + 10.0);
    let strong = scores.iter().filter(|s| **s >= strong_floor).count();

    // Sparse-only can never corroborate, so its best possible evidence — a clean
    // keyword rank-1 — counts as a strong top there.
    let top_is_strong = if embeddings {
        top >= 1.5 * rank1
    } else {
        top >= rank1
    };

    let level = if strong == 0 {
        Confidence::Low
    } else if n == 1 {
        // A single chunk of evidence is never High, however well it scored.
        if top >= rank1 {
            Confidence::Medium
        } else {
            Confidence::Low
        }
    } else if top_is_strong && strong >= 3 && n * 2 >= top_k && gap <= 3.0 {
        Confidence::High
    } else {
        Confidence::Medium
    };

    let basis = match level {
        Confidence::High => format!("{strong} strong matches"),
        Confidence::Medium if n == 1 => "a single strong match — uncorroborated".to_owned(),
        Confidence::Medium if !top_is_strong => format!("{n} moderate matches"),
        Confidence::Medium if gap > 3.0 => "one dominant match, weak support".to_owned(),
        Confidence::Medium => format!(
            "only {strong} strong match{}",
            if strong == 1 { "" } else { "es" }
        ),
        Confidence::Low => {
            if n <= 2 {
                "few weak matches — the index may not cover this".to_owned()
            } else {
                "only weak matches — the index may not cover this".to_owned()
            }
        }
    };

    Some(ConfidenceReport {
        level,
        basis,
        inputs: ConfidenceInputs {
            hit_count: n,
            top_k,
            top_score: top,
            median_score: median,
            gap,
            strong_hits: strong,
            strong_floor,
            embeddings,
        },
        uncovered: None,
    })
}

/// [`assess_confidence`] wired to a [`QaConfig`]: embeddings were available exactly
/// when the mode embedded the query (everything but sparse-only).
fn confidence_for(hits: &[SearchHit], cfg: &QaConfig) -> Option<ConfidenceReport> {
    assess_confidence(
        hits,
        cfg.top_k,
        cfg.rrf_k,
        !matches!(cfg.mode, HybridMode::Sparse),
    )
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
    /// Agentic progress: hop number (1-based) + the query being searched this hop.
    /// Emitted only by [`answer_agentic_stream`]; one-shot streams never produce it.
    Step(usize, String),
}

/// Configuration for the Q&A pipeline.
#[derive(Clone)]
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
    // Belt-and-suspenders: drop any content-free stub chunk that slipped past the SQL filter
    // (e.g. the ANN dense arm returns ids straight from the HNSW index without running it),
    // so a "File: icon.png" placeholder can never surface as an answer source.
    hits.retain(|h| !indexa_core::store::is_stub_chunk(&h.text));
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

/// Human-readable name for a retrieval mode.
fn mode_label(m: &HybridMode) -> &'static str {
    match m {
        HybridMode::Rrf => "RRF",
        HybridMode::Sparse => "sparse",
        HybridMode::Dense => "dense",
    }
}

/// One stage of the retrieval pipeline, captured for `indexa ask --explain`.
#[derive(Debug)]
pub struct RetrievalStage {
    /// What this stage represents, e.g. "sparse (BM25)", "dense (cosine)", "fused (RRF) + weights".
    pub label: String,
    /// The hits this stage produced, in rank order (with `rrf_score` populated).
    pub hits: Vec<SearchHit>,
}

/// A retrieval trace for `indexa ask --explain`: the config used plus each pipeline
/// stage's ranked hits, so a user can see *why* the answer drew on the sources it did.
#[derive(Debug)]
pub struct RetrievalTrace {
    pub question: String,
    pub mode: String,
    pub top_k: usize,
    pub rrf_k: f32,
    pub rerank: bool,
    pub use_weights: bool,
    pub scope: Option<String>,
    pub stages: Vec<RetrievalStage>,
}

/// Build a [`RetrievalTrace`] for `indexa ask --explain` — a diagnostic view of the
/// retrieval pipeline that feeds [`answer`]. Read-only: it runs the same `retrieve`
/// (fused + boosts) and optional rerank the answer path uses, and additionally surfaces
/// the per-retriever sparse-only and dense-only rankings so a user can see how each
/// contributes. Does not synthesize an answer (the caller does that separately if wanted).
pub async fn explain_retrieval(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<RetrievalTrace> {
    // Embed once (skip for sparse-only mode), reused across the dense + fused stages.
    let query_vec = match cfg.mode {
        HybridMode::Sparse => None,
        _ => Some(embedder.embed(question).await?),
    };

    let mut stages: Vec<RetrievalStage> = Vec::new();

    // Per-retriever breakdown + the actual fused result, in one sync store scope so the
    // `&Store` never crosses an `.await` (keeps this future `Send`).
    let fused = {
        let store = Store::open(db_path)?;

        // Sparse (BM25) alone — what keyword matching found.
        if let Ok(sparse) = store.hybrid_search_with_ann(
            question,
            None,
            &HybridMode::Sparse,
            cfg.scope.as_deref(),
            cfg.top_k,
            cfg.rrf_k,
            ann,
        ) {
            stages.push(RetrievalStage {
                label: "sparse (BM25)".to_owned(),
                hits: sparse,
            });
        }

        // Dense (cosine) alone — what semantic matching found (needs a query vector).
        if let Some(qv) = query_vec.as_deref() {
            if let Ok(dense) = store.hybrid_search_with_ann(
                question,
                Some(qv),
                &HybridMode::Dense,
                cfg.scope.as_deref(),
                cfg.top_k,
                cfg.rrf_k,
                ann,
            ) {
                stages.push(RetrievalStage {
                    label: "dense (cosine)".to_owned(),
                    hits: dense,
                });
            }
        }

        // The real fused + boosted result that feeds synthesis (exactly what `answer` uses).
        retrieve(&store, question, query_vec.as_deref(), cfg, ann)?
    };

    let mut final_label = format!("fused ({})", mode_label(&cfg.mode));
    if cfg.use_weights {
        final_label.push_str(" + weights");
    }
    // Optional rerank (async, fails open) — mirrors `retrieve_and_rerank`.
    let final_hits = if cfg.rerank && !fused.is_empty() {
        final_label.push_str(" + rerank");
        apply_rerank(&LlmReranker::new(llm), question, fused).await
    } else {
        fused
    };
    stages.push(RetrievalStage {
        label: final_label,
        hits: final_hits,
    });

    Ok(RetrievalTrace {
        question: question.to_owned(),
        mode: mode_label(&cfg.mode).to_owned(),
        top_k: cfg.top_k,
        rrf_k: cfg.rrf_k,
        rerank: cfg.rerank,
        use_weights: cfg.use_weights,
        scope: cfg.scope.clone(),
        stages,
    })
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

/// Streaming agentic Q&A: the [`answer_agentic`] hop loop, then a streamed synthesis.
/// Emits `Step` chunks (one per hop, so a UI can show "🔍 searching …" progress), then
/// `Sources` once, then `Fragment`s as the model generates — mirroring
/// [`answer_stream_with_ann`] for the synthesis half. The web SSE handler uses this when
/// the caller requests agentic mode.
#[allow(clippy::too_many_arguments)]
pub async fn answer_agentic_stream(
    db_path: &Path,
    embedder: &dyn Embedder,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
    on_chunk: &mut (dyn FnMut(AnswerChunk) + Send),
) -> Result<Answer> {
    // Hop loop — each hop surfaces a Step chunk. The on_step closure's borrow of
    // on_chunk is confined to this block so on_chunk is free for synthesis below.
    let hits = {
        let mut on_step =
            |step: usize, query: &str| on_chunk(AnswerChunk::Step(step, query.to_owned()));
        agentic_retrieve(db_path, embedder, llm, question, cfg, ann, &mut on_step).await?
    };

    if hits.is_empty() {
        let ans = no_match_answer(question);
        on_chunk(AnswerChunk::Sources(Vec::new()));
        on_chunk(AnswerChunk::Fragment(ans.answer.clone()));
        return Ok(ans);
    }

    // Confidence is a property of the retrieval pool, fixed before synthesis.
    let confidence = confidence_for(&hits, cfg);
    let (context, sources) = pack_context(&hits, cfg.context_budget);
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

    // Confidence is a property of the retrieval pool, fixed before synthesis.
    let confidence = confidence_for(&hits, cfg);
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
        confidence,
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

/// The shared no-match answer (identical across CLI, web, MCP). Deliberately carries
/// no confidence label — the message already says the index has nothing.
fn no_match_answer(question: &str) -> Answer {
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
pub(crate) async fn synthesize_from_hits(
    hits: Vec<SearchHit>,
    llm: &dyn Generator,
    question: &str,
    cfg: &QaConfig,
) -> Result<Answer> {
    // Confidence is a property of the retrieval pool, fixed before synthesis
    // (rerank only reorders; assess_confidence sorts scores internally).
    let confidence = confidence_for(&hits, cfg);
    let (context, sources) = pack_context(&hits, cfg.context_budget);
    let prompt = build_prompt(question, &context);
    let answer_text = llm.generate(&prompt).await?;
    Ok(Answer {
        question: question.to_owned(),
        answer: answer_text.trim().to_owned(),
        sources,
        confidence,
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
        // The over-budget chunk is cut and explicitly marked so the synthesizer knows.
        assert!(ctx.contains("truncated"));
    }

    #[test]
    fn build_prompt_contains_question_and_context() {
        let prompt = build_prompt("what is 2+2?", "some context");
        assert!(prompt.contains("what is 2+2?"));
        assert!(prompt.contains("some context"));
    }

    // ── assess_confidence (retrieval-shape classifier) ─────────────────────────

    /// Minimal hit with a given fused score (the classifier only reads scores).
    fn scored_hit(i: i64, score: f64) -> SearchHit {
        SearchHit {
            chunk_id: i,
            entry_path: format!("/doc{i}.md"),
            seq: 0,
            heading: String::new(),
            text: "x".to_owned(),
            rrf_score: score,
        }
    }

    fn scored_hits(scores: &[f64]) -> Vec<SearchHit> {
        scores
            .iter()
            .enumerate()
            .map(|(i, s)| scored_hit(i as i64, *s))
            .collect()
    }

    #[test]
    fn confidence_empty_pool_is_none() {
        // The zero-hit short-circuit owns that message; no confidence label on it.
        assert!(assess_confidence(&[], 8, 60.0, true).is_none());
    }

    #[test]
    fn confidence_one_weak_hit_is_low() {
        // Single hit deep in one list: 1/(60+40), below the strong floor 1/70.
        let r = assess_confidence(&scored_hits(&[0.010]), 8, 60.0, true).unwrap();
        assert_eq!(r.level, Confidence::Low);
        assert!(r.basis.contains("may not cover"), "basis: {}", r.basis);
        assert_eq!(r.inputs.strong_hits, 0);
        assert!(r.uncovered.is_none(), "phase-2 placeholder stays None");
    }

    #[test]
    fn confidence_many_strong_corroborated_is_high() {
        // 8 hits, top at 2/(61) (rank-1 in both lists), pool above the strong floor.
        let r = assess_confidence(
            &scored_hits(&[0.0328, 0.0301, 0.028, 0.020, 0.018, 0.016, 0.015, 0.0148]),
            8,
            60.0,
            true,
        )
        .unwrap();
        assert_eq!(r.level, Confidence::High);
        assert_eq!(r.basis, "8 strong matches");
        assert!(r.inputs.gap <= 3.0);
    }

    #[test]
    fn confidence_moderate_uncorroborated_pool_is_medium() {
        // Decent single-list hits but no chunk both retrievers agree on near the top.
        let r = assess_confidence(
            &scored_hits(&[0.0164, 0.0158, 0.0150, 0.0145]),
            8,
            60.0,
            true,
        )
        .unwrap();
        assert_eq!(r.level, Confidence::Medium);
        assert_eq!(r.basis, "4 moderate matches");
    }

    #[test]
    fn confidence_single_strong_hit_caps_at_medium() {
        // One chunk of evidence is never High, however well it scored.
        let r = assess_confidence(&scored_hits(&[0.033]), 8, 60.0, true).unwrap();
        assert_eq!(r.level, Confidence::Medium);
        assert!(r.basis.contains("single"), "basis: {}", r.basis);
    }

    #[test]
    fn confidence_dominant_top_over_weak_pool_is_medium() {
        // gap > 3 (e.g. a weight-boosted top): the pool's strength is illusory.
        let r = assess_confidence(
            &scored_hits(&[0.050, 0.0145, 0.0144, 0.0143, 0.001, 0.001]),
            8,
            60.0,
            true,
        )
        .unwrap();
        assert_eq!(r.level, Confidence::Medium);
        assert_eq!(r.basis, "one dominant match, weak support");
    }

    #[test]
    fn confidence_all_weak_pool_is_low() {
        // Plenty of hits, none reaching top-10-of-a-list mass.
        let r = assess_confidence(
            &scored_hits(&[0.012, 0.011, 0.011, 0.010, 0.010, 0.009]),
            8,
            60.0,
            true,
        )
        .unwrap();
        assert_eq!(r.level, Confidence::Low);
        assert!(r.basis.contains("may not cover"), "basis: {}", r.basis);
    }

    #[test]
    fn confidence_sparse_clean_rank1_counts_without_corroboration() {
        // Sparse-only can't corroborate; a clean keyword rank-1 top still qualifies.
        let r = assess_confidence(
            &scored_hits(&[0.0164, 0.0161, 0.0156, 0.0152, 0.0149, 0.0147]),
            8,
            60.0,
            false,
        )
        .unwrap();
        assert_eq!(r.level, Confidence::High);
        assert!(!r.inputs.embeddings);
    }

    #[test]
    fn confidence_is_order_independent() {
        // Reranked (reordered) hits must classify identically: scores are sorted internally.
        let asc = scored_hits(&[0.0148, 0.016, 0.020, 0.0328, 0.028, 0.018, 0.015, 0.0301]);
        let r = assess_confidence(&asc, 8, 60.0, true).unwrap();
        assert_eq!(r.level, Confidence::High);
        assert_eq!(r.inputs.top_score, 0.0328);
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
        assert!(
            ans.confidence.is_none(),
            "the no-match short-circuit carries no confidence label"
        );
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
        // One sparse rank-1 chunk: confidence present, capped at Medium (single hit).
        let conf = ans.confidence.expect("hits ⇒ a confidence report");
        assert_eq!(conf.level, Confidence::Medium);
        assert!(!conf.inputs.embeddings, "sparse mode never embedded");
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
                AnswerChunk::Step(..) => unreachable!("one-shot stream emits no Step"),
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
                AnswerChunk::Step(..) => unreachable!("one-shot stream emits no Step"),
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
    fn stub_chunks_are_excluded_from_retrieval() {
        use indexa_core::store::is_stub_chunk;
        // Truth table for the shared helper.
        assert!(is_stub_chunk("File: Square44x44Logo.png"));
        assert!(is_stub_chunk("Image: photo.jpg"));
        assert!(is_stub_chunk("Media file: clip.mp4"));
        assert!(!is_stub_chunk("Indexa is the local context engine for AI."));
        // A long line that merely starts with the prefix is real content, not a stub.
        assert!(!is_stub_chunk(&format!("File: {}", "x".repeat(90))));

        // A content-free image stub alongside a real chunk; a query matching both must
        // surface only the real one (filtered in SQL + the retrieve() guard).
        let (_d, path) = temp_index_with_chunks(&[
            ("/icons/logo.png", "File: logo.png"),
            (
                "/docs/brand.md",
                "The logo file is the brand mark used across the app.",
            ),
        ]);
        let store = Store::open(&path).unwrap();
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            top_k: 10,
            ..QaConfig::default()
        };
        let hits = retrieve(&store, "logo", None, &cfg, None).unwrap();
        assert!(!hits.is_empty(), "the real chunk should match 'logo'");
        assert!(
            hits.iter().all(|h| !is_stub_chunk(&h.text)),
            "stub chunk leaked into retrieval: {:?}",
            hits.iter().map(|h| h.text.clone()).collect::<Vec<_>>()
        );
        assert!(hits.iter().any(|h| h.entry_path == "/docs/brand.md"));
    }

    #[test]
    fn scoped_retrieval_limits_to_the_path_prefix() {
        // Two files under different dirs; scoping to one dir must exclude the other.
        let (_d, path) = temp_index_with_chunks(&[
            (
                "/src/auth.rs",
                "authentication token refresh and session handling",
            ),
            ("/docs/auth.md", "authentication overview for end users"),
        ]);
        let store = Store::open(&path).unwrap();
        let cfg = QaConfig {
            mode: HybridMode::Sparse,
            top_k: 10,
            scope: Some("/src".to_owned()),
            ..QaConfig::default()
        };
        let hits = retrieve(&store, "authentication", None, &cfg, None).unwrap();
        assert!(
            !hits.is_empty(),
            "scoped query should still match in-scope content"
        );
        assert!(
            hits.iter().all(|h| h.entry_path.starts_with("/src")),
            "out-of-scope chunk leaked: {:?}",
            hits.iter()
                .map(|h| h.entry_path.clone())
                .collect::<Vec<_>>()
        );
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

    #[tokio::test]
    async fn agentic_stream_emits_steps_before_sources_and_answer() {
        let (_d, path) = temp_index_with_chunks(&[
            ("/a.md", "alpha subsystem overview and design"),
            ("/b.md", "beta subsystem error handling details"),
        ]);
        let gen_calls = Arc::new(AtomicUsize::new(0));
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

        let mut step_queries: Vec<String> = Vec::new();
        let mut sources_len: Option<usize> = None;
        let mut frags = String::new();
        let mut order: Vec<&str> = Vec::new();
        let answer = {
            let mut on_chunk = |c: AnswerChunk| match c {
                AnswerChunk::Step(_n, q) => {
                    step_queries.push(q);
                    order.push("step");
                }
                AnswerChunk::Sources(s) => {
                    sources_len = Some(s.len());
                    order.push("sources");
                }
                AnswerChunk::Fragment(t) => {
                    frags.push_str(&t);
                    order.push("fragment");
                }
            };
            answer_agentic_stream(&path, &embedder, &llm, "alpha", &cfg, None, &mut on_chunk)
                .await
                .unwrap()
        };

        assert_eq!(answer.answer, "Both covered [1][2].");
        assert_eq!(frags, "Both covered [1][2].");
        assert_eq!(step_queries, vec!["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(sources_len, Some(2));
        // Every `step` must arrive before the first `sources`/`fragment`.
        let first_answer = order.iter().position(|k| *k != "step").unwrap();
        assert!(order[..first_answer].iter().all(|k| *k == "step"));
        assert_eq!(order[first_answer], "sources");
    }
}
