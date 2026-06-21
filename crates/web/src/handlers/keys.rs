use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::config;

use crate::dto::{err_json, KeyRequest, KeysStatus};
use crate::AppState;

pub(crate) async fn api_keys_get(State(state): State<AppState>) -> Json<KeysStatus> {
    let keys = &state.config.api_keys;
    Json(KeysStatus {
        openai_set: keys.openai.as_deref().is_some_and(|k| !k.is_empty()),
        anthropic_set: keys.anthropic.as_deref().is_some_and(|k| !k.is_empty()),
        google_set: keys.google.as_deref().is_some_and(|k| !k.is_empty()),
    })
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

    let cfg_path = config::default_config_path();
    // A parse error must NOT silently fall back to a default config — saving that would
    // OVERWRITE the user's config.toml (wiping every other setting AND existing API keys).
    // A missing file still loads as default (config::load returns Ok(default)), so first-time
    // key entry works; only a present-but-unparseable config is refused. Mirrors the
    // api_config_* handlers (handlers/config.rs).
    let mut cfg = match config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config exists but failed to parse; refusing to overwrite it: {e:#}"),
            )
        }
    };

    let key_val = if body.key.is_empty() {
        None
    } else {
        Some(body.key.clone())
    };
    match body.provider.as_str() {
        "openai" => cfg.api_keys.openai = key_val,
        "anthropic" => cfg.api_keys.anthropic = key_val,
        "google" => cfg.api_keys.google = key_val,
        _ => return err_json(StatusCode::BAD_REQUEST, "unknown provider"),
    }

    // Never log key material — log only the provider name.
    let provider = &body.provider;
    let _ = state.config.as_ref(); // keep state referenced
    match config::save(&cfg, &cfg_path) {
        Ok(()) => {
            tracing::info!("API key updated for provider={provider}");
            Json(serde_json::json!({"saved": true, "restart_required": true})).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
