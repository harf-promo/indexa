use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use indexa_core::config::RetrievalConfig;
use indexa_core::store::{AnnIndex, Store};
use indexa_query::{AnswerChunk, AnswerImpact, PriorTurn, QaConfig};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::dto::{
    err_json, AskConfidence, AskRequest, AskResponse, AskSource, ExplainHit, ExplainResponse,
    ExplainStage,
};
use crate::AppState;

/// Build the Q&A config from the server's retrieval settings (shared by both ask handlers).
/// `body.scope` restricts retrieval to a path prefix (the sidebar selection) — the same
/// `QaConfig.scope` the CLI `--scope` and MCP `ask {scope}` already drive. An empty string
/// means "whole index", so it's filtered out (an empty prefix would otherwise match nothing
/// meaningful and only adds a no-op LIKE).
fn qa_config(state: &AppState, body: &AskRequest) -> QaConfig {
    qa_config_from(&state.config.retrieval, body.scope.as_deref(), body.top_k)
}

/// Pure field mapping from the server's [`RetrievalConfig`] + the request scope to a
/// [`QaConfig`]. Split out of [`qa_config`] (which only adds the `AppState` lookup) so the
/// mapping and the scope normalization are unit-testable without building a full `AppState`.
/// `top_k` overrides the server's retrieval breadth (capped at 100); `None` ⇒ the config default.
fn qa_config_from(r: &RetrievalConfig, scope: Option<&str>, top_k: Option<usize>) -> QaConfig {
    QaConfig {
        top_k: top_k.map(|k| k.min(100)).unwrap_or(r.top_k),
        mode: r.hybrid.clone(),
        context_budget: r.context_budget,
        rrf_k: r.rrf_k as f32,
        summary_weight: r.summary_weight,
        summary_depth_alpha: r.summary_depth_alpha,
        rerank: r.rerank,
        rerank_backend: r.rerank_backend.clone(),
        use_weights: r.use_weights,
        use_recency_weight: r.recency_boost,
        recency_days: r.recency_days,
        max_steps: r.agentic_max_steps,
        mmr_lambda: r.mmr_lambda,
        archive_segments: r.archive_segments.clone(),
        archive_penalty: r.archive_penalty,
        broad_per_file_cap: r.broad_per_file_cap,
        scope: scope
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

/// Whether agentic retrieval is requested for this call: the request's `agentic` flag,
/// or the server's `[retrieval] agentic` default when unset.
fn agentic_requested(state: &AppState, body: &AskRequest) -> bool {
    agentic_from(&state.config.retrieval, body.agentic)
}

/// Pure twin of [`agentic_requested`]: the per-request flag overrides the server default.
fn agentic_from(r: &RetrievalConfig, requested: Option<bool>) -> bool {
    requested.unwrap_or(r.agentic)
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
    // Conversational Ask: load this session's recent turns (empty for a stateless ask).
    let history = load_history(
        &state.db_path,
        body.session_id.as_deref(),
        qa_cfg.scope.as_deref(),
    );

    // Single shared, Send-safe pipeline (embed → scoped retrieve → optional
    // rerank → synthesize). `answer_with_ann_history` opens its own short-lived read
    // connection from `db_path`, so we don't hold the shared store mutex across the LLM
    // round-trips. Empty-hit short-circuit lives inside it. Agentic mode runs the
    // bounded plan→search→refine loop first (progress isn't surfaced on this buffered
    // endpoint — the SSE endpoint streams the per-hop steps).
    let synthesize = body.synthesize.unwrap_or(true);
    let result = if !synthesize {
        // Retrieval-only: return the packed slice for the client to synthesize itself.
        indexa_query::answer_retrieval_only_history(
            &state.db_path,
            state.embedder.as_ref(),
            state.llm.as_ref(),
            &body.question,
            &qa_cfg,
            ann.as_deref(),
            &history,
        )
        .await
    } else if agentic {
        indexa_query::answer_agentic_history(
            &state.db_path,
            state.embedder.as_ref(),
            state.llm.as_ref(),
            &body.question,
            &qa_cfg,
            &history,
            &mut |_step, _query| {},
        )
        .await
    } else {
        indexa_query::answer_with_ann_history(
            &state.db_path,
            state.embedder.as_ref(),
            state.llm.as_ref(),
            &body.question,
            &qa_cfg,
            ann.as_deref(),
            &history,
        )
        .await
    };
    match result {
        Ok(mut answer) => {
            // Stamp the local synthesis model (transparency); left unset on retrieval-only.
            if answer.synthesized {
                answer.model = Some(format!(
                    "{}/{}",
                    state.config.describer.provider, state.config.describer.model
                ));
            }
            // Only surface the readout when it's a real win (cited files existed and serving was
            // smaller) — never a misleading "0% saved" badge.
            let impact = record_ask_usage(&state.db_path, &answer, body.session_id.as_deref())
                .filter(AnswerImpact::is_meaningful);
            // Persist this turn (best-effort) and echo the session id back. Skip on the
            // retrieval-only path — the slice is not an answer and would poison history.
            if let Some(id) = body.session_id.as_deref() {
                if answer.synthesized {
                    append_turn_best_effort(&state.db_path, id, &body.question, &answer);
                }
            }
            Json(AskResponse {
                confidence: into_ask_confidence(answer.confidence.as_ref()),
                answer: answer.answer,
                sources: answer.sources.into_iter().map(into_ask_source).collect(),
                impact,
                session_id: body.session_id,
                synthesized: answer.synthesized,
                model: answer.model,
            })
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Record best-effort token-savings telemetry AND return the per-answer impact so the handler
/// can surface it to the user. A recording failure must never fail (or delay-fail) the answer,
/// so a store-open error just yields `None`. Opens its own short-lived connection (the buffered
/// handler never locked the shared store; the stream task outlives the request scope), then
/// delegates the shared accounting (served bytes + counterfactual + usage row, tagged with the
/// conversation `session_id`) to `indexa_query::record_ask_impact`.
fn record_ask_usage(
    db_path: &std::path::Path,
    answer: &indexa_query::Answer,
    session_id: Option<&str>,
) -> Option<AnswerImpact> {
    let mut s = Store::open(db_path)
        .map_err(|e| tracing::debug!("usage telemetry skipped: {e:#}"))
        .ok()?;
    Some(indexa_query::record_ask_impact(
        &mut s, "web", answer, session_id,
    ))
}

/// How many recent turns of a conversation to fold into the prompt. Small by design —
/// the per-turn budget clamp in the qa pipeline trims further if they don't fit.
const HISTORY_TURNS: usize = 6;

/// Conversational Ask: ensure the session row exists and load its recent turns as
/// [`PriorTurn`]s for the qa pipeline. Sync (own short-lived connection, dropped before any
/// `.await`) and fail-open — a store error just yields no history (a stateless ask). `None`
/// session_id ⇒ empty, the single-shot default.
fn load_history(
    db_path: &std::path::Path,
    session_id: Option<&str>,
    scope: Option<&str>,
) -> Vec<PriorTurn> {
    let Some(id) = session_id else {
        return Vec::new();
    };
    let mut s = match Store::open(db_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("conversation history skipped: {e:#}");
            return Vec::new();
        }
    };
    if let Err(e) = s.ensure_session(id, scope) {
        tracing::debug!("ensure_session skipped: {e:#}");
        return Vec::new();
    }
    s.recent_turns(id, HISTORY_TURNS)
        .map(|turns| {
            turns
                .into_iter()
                .map(|t| PriorTurn {
                    question: t.question,
                    answer: t.answer,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Persist a completed turn, best-effort (a failure must never fail the user's answer).
/// Serializes the citations to the `sources_json` column the store keeps opaque.
fn append_turn_best_effort(
    db_path: &std::path::Path,
    session_id: &str,
    question: &str,
    answer: &indexa_query::Answer,
) {
    let sources_json = serde_json::to_string(
        &answer
            .sources
            .iter()
            .map(|s| {
                serde_json::json!({ "path": s.path, "heading": s.heading, "snippet": s.snippet })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned());
    match Store::open(db_path) {
        Ok(mut s) => {
            if let Err(e) = s.append_turn(session_id, question, &answer.answer, &sources_json) {
                tracing::debug!("append_turn skipped: {e:#}");
            }
        }
        Err(e) => tracing::debug!("append_turn skipped: {e:#}"),
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
    let session_id = body.session_id;
    let synthesize = body.synthesize.unwrap_or(true);
    // The local synthesis model id, captured for the transparency stamp (the task can't borrow
    // `state`). Used only when `synthesize` (left unset on the retrieval-only path).
    let model_id = format!(
        "{}/{}",
        state.config.describer.provider, state.config.describer.model
    );
    // Conversational Ask: recent turns folded into the prompt (empty for a stateless ask).
    let history = load_history(&db_path, session_id.as_deref(), qa_cfg.scope.as_deref());

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
        // Retrieval-only has nothing to stream: emit the citations, then the slice as a single
        // fragment, so the client renders identically to a one-shot answer.
        let result = if !synthesize {
            match indexa_query::answer_retrieval_only_history(
                &db_path,
                embedder.as_ref(),
                llm.as_ref(),
                &question,
                &qa_cfg,
                ann.as_deref(),
                &history,
            )
            .await
            {
                Ok(ans) => {
                    on_chunk(AnswerChunk::Sources(ans.sources.clone()));
                    on_chunk(AnswerChunk::Fragment(ans.answer.clone()));
                    Ok(ans)
                }
                Err(e) => Err(e),
            }
        } else if agentic {
            indexa_query::answer_agentic_stream_history(
                &db_path,
                embedder.as_ref(),
                llm.as_ref(),
                &question,
                &qa_cfg,
                ann.as_deref(),
                &history,
                &mut on_chunk,
            )
            .await
        } else {
            indexa_query::answer_stream_with_ann_history(
                &db_path,
                embedder.as_ref(),
                llm.as_ref(),
                &question,
                &qa_cfg,
                ann.as_deref(),
                &history,
                &mut on_chunk,
            )
            .await
        };

        let terminal = match result {
            Ok(mut answer) => {
                // Stamp the local synthesis model (transparency); left unset on retrieval-only.
                if answer.synthesized {
                    answer.model = Some(model_id);
                }
                let impact = record_ask_usage(&db_path, &answer, session_id.as_deref())
                    .filter(AnswerImpact::is_meaningful);
                // Persist this turn (best-effort) and echo the session id on `done`. Skip on the
                // retrieval-only path — the slice is not an answer and would poison history.
                if let Some(id) = session_id.as_deref() {
                    if answer.synthesized {
                        append_turn_best_effort(&db_path, id, &question, &answer);
                    }
                }
                // Confidence + impact ride the terminal `done` event (both belong to the whole
                // answer). Additive: each absent on the no-match path and on older servers, so
                // clients must tolerate either field missing.
                let mut done = serde_json::Map::new();
                done.insert("type".into(), serde_json::json!("done"));
                done.insert("synthesized".into(), serde_json::json!(answer.synthesized));
                if let Some(m) = &answer.model {
                    done.insert("model".into(), serde_json::json!(m));
                }
                if let Some(c) = into_ask_confidence(answer.confidence.as_ref()) {
                    done.insert(
                        "confidence".into(),
                        serde_json::to_value(c).unwrap_or_default(),
                    );
                }
                if let Some(i) = impact {
                    done.insert(
                        "impact".into(),
                        serde_json::json!({
                            "served_bytes": i.served_bytes,
                            "counterfactual_bytes": i.counterfactual_bytes,
                            "saved_percent": i.saved_percent(),
                        }),
                    );
                }
                if let Some(id) = &session_id {
                    done.insert("session_id".into(), serde_json::json!(id));
                }
                serde_json::Value::Object(done)
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

/// `POST /api/ask/explain` — the retrieval trace for a question ("why these sources"): per-stage
/// ranked hits (sparse / dense / fused) with scores. Mirrors the CLI `ask --explain`. Read-only and
/// answer-free; runs the same scoped retrieve the answer path uses, on demand from the UI.
pub(crate) async fn api_ask_explain(
    State(state): State<AppState>,
    Json(body): Json<AskRequest>,
) -> Response {
    let qa_cfg = qa_config(&state, &body);
    let ann = ensure_ann(&state).await;
    match indexa_query::explain_retrieval(
        &state.db_path,
        state.embedder.as_ref(),
        state.llm.as_ref(),
        &body.question,
        &qa_cfg,
        ann.as_deref(),
    )
    .await
    {
        Ok(trace) => Json(ExplainResponse {
            question: trace.question,
            mode: trace.mode,
            top_k: trace.top_k,
            rrf_k: trace.rrf_k,
            rerank: trace.rerank,
            use_weights: trace.use_weights,
            scope: trace.scope,
            stages: trace
                .stages
                .into_iter()
                .map(|st| ExplainStage {
                    label: st.label,
                    hits: st
                        .hits
                        .into_iter()
                        .enumerate()
                        .map(|(i, h)| ExplainHit {
                            rank: i + 1,
                            path: h.entry_path,
                            heading: h.heading,
                            score: h.rrf_score,
                        })
                        .collect(),
                })
                .collect(),
        })
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn into_ask_confidence(c: Option<&indexa_query::ConfidenceReport>) -> Option<AskConfidence> {
    c.map(|c| AskConfidence {
        level: c.level.as_str(),
        basis: c.basis.clone(),
        uncovered: c.uncovered.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{agentic_from, qa_config_from};
    use indexa_core::config::RetrievalConfig;

    #[test]
    fn agentic_flag_overrides_server_default_both_ways() {
        // Request flag wins regardless of the server default.
        let off = RetrievalConfig {
            agentic: false,
            ..Default::default()
        };
        assert!(
            agentic_from(&off, Some(true)),
            "explicit true overrides default-off"
        );
        let on = RetrievalConfig {
            agentic: true,
            ..Default::default()
        };
        assert!(
            !agentic_from(&on, Some(false)),
            "explicit false overrides default-on"
        );
    }

    #[test]
    fn agentic_unset_falls_back_to_server_default() {
        let on = RetrievalConfig {
            agentic: true,
            ..Default::default()
        };
        assert!(agentic_from(&on, None), "None ⇒ server default (on)");
        let off = RetrievalConfig {
            agentic: false,
            ..Default::default()
        };
        assert!(!agentic_from(&off, None), "None ⇒ server default (off)");
    }

    #[test]
    fn qa_config_blank_scope_becomes_none() {
        let r = RetrievalConfig::default();
        // Empty string and whitespace both mean "whole index" → no scope filter.
        assert!(qa_config_from(&r, None, None).scope.is_none());
        assert!(qa_config_from(&r, Some(""), None).scope.is_none());
        assert!(qa_config_from(&r, Some("   "), None).scope.is_none());
        // A real path is kept, trimmed of surrounding whitespace.
        assert_eq!(
            qa_config_from(&r, Some("  /src/auth  "), None)
                .scope
                .as_deref(),
            Some("/src/auth")
        );
    }

    #[test]
    fn qa_config_carries_server_retrieval_settings() {
        let r = RetrievalConfig {
            top_k: 17,
            context_budget: 1234,
            recency_boost: true,  // maps to QaConfig.use_recency_weight
            agentic_max_steps: 4, // maps to QaConfig.max_steps
            mmr_lambda: 0.25,
            ..Default::default()
        };
        let cfg = qa_config_from(&r, None, None);
        assert_eq!(cfg.top_k, 17);
        assert_eq!(cfg.context_budget, 1234);
        assert!(
            cfg.use_recency_weight,
            "recency_boost maps to use_recency_weight"
        );
        assert_eq!(cfg.max_steps, 4, "agentic_max_steps maps to max_steps");
        assert_eq!(cfg.mmr_lambda, 0.25);
    }

    #[test]
    fn qa_config_top_k_override_caps_and_falls_back() {
        let r = RetrievalConfig {
            top_k: 8,
            ..Default::default()
        };
        // None ⇒ server default.
        assert_eq!(qa_config_from(&r, None, None).top_k, 8);
        // Explicit override wins.
        assert_eq!(qa_config_from(&r, None, Some(25)).top_k, 25);
        // Capped at 100.
        assert_eq!(qa_config_from(&r, None, Some(9999)).top_k, 100);
    }
}
