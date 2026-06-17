//! Agentic ("self-ask") multi-step Q&A: a bounded iterative-retrieval loop that searches,
//! asks the model whether a part of the question is still uncovered, and searches again
//! before synthesizing. Opt-in; fails open to a single hop on any unparseable decision.

use std::path::Path;

use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{AnnIndex, SearchHit, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;

use super::confidence::confidence_for;
use super::retrieve::{build_project_overview, is_broad_intent, retrieve};
use super::synthesize::{build_prompt, no_match_answer, pack_context, synthesize_from_hits};
use super::{Answer, AnswerChunk, QaConfig};

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
/// the default [`answer`](super::answer) stays one-shot.
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
    let (hits, overview) =
        agentic_retrieve(db_path, embedder, llm, question, cfg, None, on_step).await?;
    if hits.is_empty() {
        return Ok(no_match_answer(question));
    }
    synthesize_from_hits(hits, overview, llm, question, cfg).await
}

/// Streaming agentic Q&A: the [`answer_agentic`] hop loop, then a streamed synthesis.
/// Emits `Step` chunks (one per hop, so a UI can show "🔍 searching …" progress), then
/// `Sources` once, then `Fragment`s as the model generates — mirroring
/// [`answer_stream_with_ann`](super::answer_stream_with_ann) for the synthesis half. The web SSE handler uses this when
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
    let (hits, overview) = {
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
    let (context, sources) = pack_context(&hits, &overview, cfg.context_budget);
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

/// The agentic hop loop: returns `(merged hit pool, project overview)`.
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
) -> Result<(Vec<SearchHit>, String)> {
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

    // Build project overview from the merged pool — same logic as retrieve_and_rerank.
    // Open a fresh Store; the borrow is dropped before returning so the future stays Send.
    let overview = {
        let overview_budget = if is_broad_intent(question) {
            cfg.context_budget * 35 / 100
        } else {
            300
        };
        let store = Store::open(db_path)?;
        build_project_overview(&store, &pool, cfg.scope.as_deref(), overview_budget)
    };

    Ok((pool, overview))
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
pub(crate) fn parse_followup(reply: &str) -> Option<String> {
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
