use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use indexa_query::{AnswerChunk, QaConfig};
use std::convert::Infallible;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::dto::{err_json, AskRequest, AskResponse, AskSource};
use crate::AppState;

/// Build the Q&A config from the server's retrieval settings (shared by both ask handlers).
fn qa_config(state: &AppState) -> QaConfig {
    QaConfig {
        top_k: state.config.retrieval.top_k,
        mode: state.config.retrieval.hybrid.clone(),
        context_budget: state.config.retrieval.context_budget,
        rrf_k: state.config.retrieval.rrf_k as f32,
        summary_weight: state.config.retrieval.summary_weight,
        summary_depth_alpha: state.config.retrieval.summary_depth_alpha,
        rerank: state.config.retrieval.rerank,
        ..QaConfig::default()
    }
}

pub(crate) async fn api_ask(
    State(state): State<AppState>,
    Json(body): Json<AskRequest>,
) -> Response {
    let qa_cfg = qa_config(&state);

    // Single shared, Send-safe pipeline (embed → scoped retrieve → optional
    // rerank → synthesize). `answer` opens its own short-lived read connection
    // from `db_path`, so we don't hold the shared store mutex across the LLM
    // round-trips. Empty-hit short-circuit lives inside `answer`.
    match indexa_query::answer(
        &state.db_path,
        state.embedder.as_ref(),
        state.llm.as_ref(),
        &body.question,
        &qa_cfg,
    )
    .await
    {
        Ok(answer) => Json(AskResponse {
            answer: answer.answer,
            sources: answer.sources.into_iter().map(into_ask_source).collect(),
        })
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
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
    let qa_cfg = qa_config(&state);
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
            };
            let _ = send_tx.send(Ok(Event::default().data(payload.to_string())));
        };

        let result = indexa_query::answer_stream(
            &db_path,
            embedder.as_ref(),
            llm.as_ref(),
            &question,
            &qa_cfg,
            &mut on_chunk,
        )
        .await;

        let terminal = match result {
            Ok(_) => serde_json::json!({ "type": "done" }),
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
