use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::config;

use crate::dto::{err_json, KeyRequest, KeysStatus};
use crate::AppState;

pub(crate) async fn api_keys_get(State(state): State<AppState>) -> Response {
    let config = match config::load(&state.config_path) {
        Ok(config) => config,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let keys = &config.api_keys;
    Json(KeysStatus {
        openai_set: keys.openai.as_deref().is_some_and(|k| !k.is_empty()),
        anthropic_set: keys.anthropic.as_deref().is_some_and(|k| !k.is_empty()),
        google_set: keys.google.as_deref().is_some_and(|k| !k.is_empty()),
    })
    .into_response()
}

pub(crate) async fn api_keys_set(
    State(state): State<AppState>,
    Json(body): Json<KeyRequest>,
) -> Response {
    // Gate: require env flag to allow writing secrets via the web UI.
    if std::env::var("INDEXA_WEB_ALLOW_KEY_EDIT").as_deref() != Ok("1") {
        return err_json(
            StatusCode::FORBIDDEN,
            "Set INDEXA_WEB_ALLOW_KEY_EDIT=1 to enable API key editing via the web UI.",
        );
    }

    let key_val = if body.key.is_empty() {
        None
    } else {
        Some(body.key.clone())
    };
    if !matches!(body.provider.as_str(), "openai" | "anthropic" | "google") {
        return err_json(StatusCode::BAD_REQUEST, "unknown provider");
    }

    // Never log key material — log only the provider name.
    let provider = body.provider.clone();
    match config::update(&state.config_path, |cfg| {
        match body.provider.as_str() {
            "openai" => cfg.api_keys.openai = key_val,
            "anthropic" => cfg.api_keys.anthropic = key_val,
            "google" => cfg.api_keys.google = key_val,
            _ => unreachable!("provider was validated above"),
        }
        Ok(())
    }) {
        Ok(_) => {
            tracing::info!("API key updated for provider={provider}");
            Json(serde_json::json!({"saved": true})).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
