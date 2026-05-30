use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_query::QaConfig;

use crate::dto::{err_json, AskRequest, AskResponse, AskSource};
use crate::AppState;

pub(crate) async fn api_ask(
    State(state): State<AppState>,
    Json(body): Json<AskRequest>,
) -> Response {
    let qa_cfg = QaConfig {
        top_k: state.config.retrieval.top_k,
        mode: state.config.retrieval.hybrid.clone(),
        context_budget: state.config.retrieval.context_budget,
        rrf_k: state.config.retrieval.rrf_k as f32,
        summary_weight: state.config.retrieval.summary_weight,
        summary_depth_alpha: state.config.retrieval.summary_depth_alpha,
        rerank: state.config.retrieval.rerank,
        ..QaConfig::default()
    };

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
            sources: answer
                .sources
                .into_iter()
                .map(|s| AskSource {
                    path: s.path,
                    heading: s.heading,
                    snippet: s.snippet,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
