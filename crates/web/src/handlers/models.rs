use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;

use crate::dto::{err_json, ModelInfo, PullRequest};
use crate::AppState;

pub(crate) async fn api_models_installed(State(state): State<AppState>) -> Response {
    let base = &state.config.describer.base_url;
    let url = format!("{base}/api/tags");
    let resp = match reqwest::Client::new().get(&url).send().await {
        Ok(r) => r,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let models: Vec<ModelInfo> = body["models"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|m| ModelInfo {
            name: m["name"].as_str().unwrap_or("").to_owned(),
            size: m["size"].as_u64().unwrap_or(0),
        })
        .collect();
    Json(models).into_response()
}

pub(crate) async fn api_models_pull(
    State(state): State<AppState>,
    Json(body): Json<PullRequest>,
) -> Response {
    let base = &state.config.describer.base_url;
    let url = format!("{base}/api/pull");
    let resp = match reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({"name": body.name, "stream": true}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_json(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    // Proxy the NDJSON stream straight through to the client.
    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(std::io::Error::other));
    Response::builder()
        .status(200)
        .header("Content-Type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap()
        .into_response()
}
