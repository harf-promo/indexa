use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use indexa_core::store::{AnnIndex, Store};
use indexa_query::{AnswerChunk, QaConfig};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::dto::{err_json, AskConfidence, AskRequest, AskResponse, AskSource};
use crate::AppState;

/// Build the Q&A config from the server's retrieval settings (shared by both ask handlers).
/// `body.scope` restricts retrieval to a path prefix (the sidebar selection) — the same
/// `QaConfig.scope` the CLI `--scope` and MCP `ask {scope}` already drive. An empty string
/// means "whole index", so it's filtered out (an empty prefix would otherwise match nothing
/// meaningful and only adds a no-op LIKE).
fn qa_config(state: &AppState, body: &AskRequest) -> QaConfig {
    QaConfig {
        top_k: state.config.retrieval.top_k,
        mode: state.config.retrieval.hybrid.clone(),
        context_budget: state.config.retrieval.context_budget,
        rrf_k: state.config.retrieval.rrf_k as f32,
        summary_weight: state.config.retrieval.summary_weight,
        summary_depth_alpha: state.config.retrieval.summary_depth_alpha,
        rerank: state.config.retrieval.rerank,
        use_weights: state.config.retrieval.use_weights,
        use_recency_weight: state.config.retrieval.recency_boost,
        recency_days: state.config.retrieval.recency_days,
        max_steps: state.config.retrieval.agentic_max_steps,
        scope: body
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

/// Whether agentic retrieval is requested for this call: the request's `agentic` flag,
/// or the server's `[retrieval] agentic` default when unset.
fn agentic_requested(state: &AppState, body: &AskRequest) -> bool {
    body.agentic.unwrap_or(state.config.retrieval.agentic)
}

/// Lazily build (and cache) the ANN index for dense retrieval, or return `None` to use the
/// brute-force cosine scan. `None` when ANN is off (`[retrieval] ann`), the index is below
/// `ann_min_chunks`, or a build/read fails. Rebuilds when the chunk watermark
/// `(count, last_indexed_at)` changes, so a `deep` job that adds or edits chunks
/// transparently refreshes the index on the next Ask. All store access uses fresh read
/// connections (WAL → concurrent reads) so the shared store mutex is never held across the
/// CPU-heavy build.
async fn ensure_ann(state: &AppState) -> Option<Arc<AnnIndex>> {
    if !state.config.retrieval.ann {
        return None;
    }
    let db_path = state.db_path.clone();
    let min_chunks = state.config.retrieval.ann_min_chunks;

    // Watermark = (chunk_count, max_chunk_id). With AUTOINCREMENT ids, max_chunk_id is
    // monotonic: any insert/edit bumps it, any delete changes the count — so a stale index
    // is always detected. (last_indexed_at was 1-second-granular and could miss a same-second
    // in-place edit.)
    let (count, max_id) = tokio::task::spawn_blocking({
        let db_path = db_path.clone();
        move || -> Option<(i64, i64)> {
            let s = Store::open(&db_path).ok()?;
            Some((s.chunk_count().ok()? as i64, s.max_chunk_id().ok()?))
        }
    })
    .await
    .ok()??;

    if (count as usize) < min_chunks {
        return None;
    }

    // Fast path: cached index still matches the watermark.
    {
        let cache = state.ann.read().await;
        if let Some(idx) = &cache.index {
            if cache.watermark == (count, max_id) {
                return Some(idx.clone());
            }
        }
    }

    // Single-flight: serialize builds so concurrent cold/stale Asks don't each allocate a
    // full index (each build transiently holds all embeddings). Re-check after acquiring —
    // another caller may have just built the current index.
    let _build_guard = state.ann_build_lock.lock().await;
    {
        let cache = state.ann.read().await;
        if let Some(idx) = &cache.index {
            if cache.watermark == (count, max_id) {
                return Some(idx.clone());
            }
        }
    }

    // Build fresh (CPU-heavy → spawn_blocking; reads on its own connection).
    let built = tokio::task::spawn_blocking(move || -> Option<AnnIndex> {
        let s = Store::open(&db_path).ok()?;
        let items = s.all_chunk_embeddings().ok()?;
        let dim = items
            .iter()
            .find(|(_, v)| !v.is_empty())
            .map(|(_, v)| v.len())?;
        Some(AnnIndex::build(&items, dim))
    })
    .await
    .ok()??;

    let idx = Arc::new(built);
    {
        let mut cache = state.ann.write().await;
        cache.index = Some(idx.clone());
        cache.watermark = (count, max_id);
    }
    Some(idx)
}

pub(crate) async fn api_ask(
    State(state): State<AppState>,
    Json(body): Json<AskRequest>,
) -> Response {
    let qa_cfg = qa_config(&state, &body);
    let agentic = agentic_requested(&state, &body);
    let ann = ensure_ann(&state).await;

    // Single shared, Send-safe pipeline (embed → scoped retrieve → optional
    // rerank → synthesize). `answer_with_ann` opens its own short-lived read connection
    // from `db_path`, so we don't hold the shared store mutex across the LLM
    // round-trips. Empty-hit short-circuit lives inside it. Agentic mode runs the
    // bounded plan→search→refine loop first (progress isn't surfaced on this buffered
    // endpoint — the SSE endpoint streams the per-hop steps).
    let result = if agentic {
        indexa_query::answer_agentic(
            &state.db_path,
            state.embedder.as_ref(),
            state.llm.as_ref(),
            &body.question,
            &qa_cfg,
            &mut |_step, _query| {},
        )
        .await
    } else {
        indexa_query::answer_with_ann(
            &state.db_path,
            state.embedder.as_ref(),
            state.llm.as_ref(),
            &body.question,
            &qa_cfg,
            ann.as_deref(),
        )
        .await
    };
    match result {
        Ok(answer) => {
            record_ask_usage(&state.db_path, &answer);
            Json(AskResponse {
                confidence: into_ask_confidence(answer.confidence.as_ref()),
                answer: answer.answer,
                sources: answer.sources.into_iter().map(into_ask_source).collect(),
            })
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Best-effort token-savings telemetry for both ask handlers — a recording
/// failure must never fail (or delay-fail) the user's answer. Opens its own
/// short-lived connection: the buffered handler never locked the shared store,
/// and the stream task outlives the request scope.
fn record_ask_usage(db_path: &std::path::Path, answer: &indexa_query::Answer) {
    let recorded = Store::open(db_path).and_then(|mut s| {
        let paths: Vec<&str> = answer.sources.iter().map(|x| x.path.as_str()).collect();
        let counterfactual = s.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
        // Served = answer + the source snippets actually delivered — counting
        // less served would inflate the savings (MCP ask counts its full
        // rendered output the same way).
        let served = answer.answer.len()
            + answer
                .sources
                .iter()
                .map(|x| x.path.len() + x.heading.len() + x.snippet.len())
                .sum::<usize>();
        s.record_tool_usage("web", "ask", served as u64, counterfactual)
    });
    if let Err(e) = recorded {
        tracing::debug!("usage telemetry skipped: {e:#}");
    }
}

/// `POST /api/ask/stream` — same pipeline as [`api_ask`] but server-sent events: one
/// `sources` event up front, then a `fragment` event per token as the model streams
/// (real streaming on Ollama; cloud/claude-code providers send one big fragment), then a
/// terminal `done` (or `error`) event. POST (not EventSource) so the question travels in
/// the body, not the URL.
///
/// Generation runs in a spawned task whose `on_chunk` closure pushes each event into an
/// mpsc channel; the response streams the receiver. The task owns cloned `Arc` handles +
/// the db path, so nothing borrows the request scope across the stream's lifetime. If the
/// client disconnects the sends fail silently and the (bounded-length) generation simply
/// runs to completion — the same as the buffered handler.
pub(crate) async fn api_ask_stream(
    State(state): State<AppState>,
    Json(body): Json<AskRequest>,
) -> impl IntoResponse {
    let qa_cfg = qa_config(&state, &body);
    let agentic = agentic_requested(&state, &body);
    let ann = ensure_ann(&state).await; // owned Arc moved into the task below
    let db_path = state.db_path.clone();
    let embedder = state.embedder.clone();
    let llm = state.llm.clone();
    let question = body.question;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Event, Infallible>>();

    tokio::spawn(async move {
        let send_tx = tx.clone();
        let mut on_chunk = move |chunk: AnswerChunk| {
            let payload = match chunk {
                AnswerChunk::Sources(srcs) => {
                    let sources: Vec<AskSource> = srcs.into_iter().map(into_ask_source).collect();
                    serde_json::json!({ "type": "sources", "sources": sources })
                }
                AnswerChunk::Fragment(text) => {
                    serde_json::json!({ "type": "fragment", "text": text })
                }
                AnswerChunk::Step(step, query) => {
                    serde_json::json!({ "type": "step", "step": step, "query": query })
                }
            };
            let _ = send_tx.send(Ok(Event::default().data(payload.to_string())));
        };

        // Agentic mode streams per-hop `step` events (from the plan→search→refine loop)
        // before the synthesized answer; one-shot mode goes straight to sources+fragments.
        let result = if agentic {
            indexa_query::answer_agentic_stream(
                &db_path,
                embedder.as_ref(),
                llm.as_ref(),
                &question,
                &qa_cfg,
                ann.as_deref(),
                &mut on_chunk,
            )
            .await
        } else {
            indexa_query::answer_stream_with_ann(
                &db_path,
                embedder.as_ref(),
                llm.as_ref(),
                &question,
                &qa_cfg,
                ann.as_deref(),
                &mut on_chunk,
            )
            .await
        };

        let terminal = match result {
            Ok(answer) => {
                record_ask_usage(&db_path, &answer);
                // Confidence rides the terminal event (it's known before synthesis but
                // belongs to the whole answer). Additive: absent on the no-match path
                // and on older servers, so clients must tolerate it missing.
                match into_ask_confidence(answer.confidence.as_ref()) {
                    Some(c) => serde_json::json!({ "type": "done", "confidence": c }),
                    None => serde_json::json!({ "type": "done" }),
                }
            }
            Err(e) => serde_json::json!({ "type": "error", "message": format!("{e:#}") }),
        };
        let _ = tx.send(Ok(Event::default().data(terminal.to_string())));
    });

    Sse::new(UnboundedReceiverStream::new(rx)).keep_alive(KeepAlive::new())
}

fn into_ask_source(s: indexa_query::SourceCitation) -> AskSource {
    AskSource {
        path: s.path,
        heading: s.heading,
        snippet: s.snippet,
    }
}

fn into_ask_confidence(c: Option<&indexa_query::ConfidenceReport>) -> Option<AskConfidence> {
    c.map(|c| AskConfidence {
        level: c.level.as_str(),
        basis: c.basis.clone(),
    })
}
