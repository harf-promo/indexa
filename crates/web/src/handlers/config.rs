use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::config;
use indexa_core::resource::ResourceProfile;

use crate::dto::{
    err_json, ConfigResponse, FeaturesRequest, FeaturesResponse, PassesRequest, ProviderRequest,
    ResourceRequest, ResourceResponse,
};
use crate::AppState;

pub(crate) async fn api_config_get(State(state): State<AppState>) -> Json<ConfigResponse> {
    let cfg = &state.config.describer;
    Json(ConfigResponse {
        passes_first: cfg.passes_first,
        passes_refresh: cfg.passes_refresh,
        passes_cap: cfg.passes_cap,
        max_children_per_summary: cfg.max_children_per_summary,
        describer_provider: cfg.provider.clone(),
        base_url: cfg.base_url.clone(),
        file_model: cfg.file_model.clone(),
        dir_model: cfg.dir_model.clone(),
        qa_model: cfg.model.clone(),
        embed_model: state.config.embedding.model.clone(),
    })
}

/// Persist describer/embedding model assignments (provider, per-role models,
/// base URL) to config.toml. Gated like `api_config_passes` — these select which
/// model runs and can point at a remote endpoint, so they are not ungated like the
/// non-secret resource profile. Only the fields present in the body are written;
/// every other config section is preserved by the load → mutate → save round-trip.
pub(crate) async fn api_config_provider_set(Json(body): Json<ProviderRequest>) -> Response {
    if std::env::var("INDEXA_WEB_ALLOW_KEY_EDIT").as_deref() != Ok("1") {
        return err_json(StatusCode::FORBIDDEN, "INDEXA_WEB_ALLOW_KEY_EDIT not set");
    }
    // Whitelist providers we actually support, so a typo can't strand the next launch.
    if let Some(p) = &body.provider {
        if !matches!(
            p.as_str(),
            "ollama" | "openai" | "anthropic" | "claude-code"
        ) {
            return err_json(StatusCode::BAD_REQUEST, format!("unknown provider: {p}"));
        }
    }

    let cfg_path = config::default_config_path();
    // Err here = the file exists but failed to parse: never overwrite it (would wipe
    // [api_keys] etc.); the user can still fix it by hand. Mirrors api_config_passes.
    let mut cfg = match config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config exists but failed to parse; refusing to overwrite it: {e:#}"),
            )
        }
    };
    // Apply only the present, non-empty fields. Empty values are ignored rather
    // than written, so a stray "" can't strand the next job with a blank model name.
    let set = |dst: &mut String, v: Option<String>| {
        if let Some(v) = v {
            let v = v.trim();
            if !v.is_empty() {
                *dst = v.to_owned();
            }
        }
    };
    if let Some(v) = body.provider {
        cfg.describer.provider = v.trim().to_owned(); // already whitelisted above
    }
    set(&mut cfg.describer.model, body.model);
    set(&mut cfg.describer.file_model, body.file_model);
    set(&mut cfg.describer.dir_model, body.dir_model);
    set(&mut cfg.describer.base_url, body.base_url);
    set(&mut cfg.embedding.model, body.embed_model);

    match config::save(&cfg, &cfg_path) {
        // restart_required: the running server holds an Arc<Config> snapshot and does
        // not hot-reload, so the change applies on the next `indexa serve`.
        Ok(_) => {
            Json(serde_json::json!({ "saved": true, "restart_required": true })).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_config_passes(
    State(state): State<AppState>,
    Json(body): Json<PassesRequest>,
) -> Response {
    if std::env::var("INDEXA_WEB_ALLOW_KEY_EDIT").as_deref() != Ok("1") {
        return err_json(StatusCode::FORBIDDEN, "INDEXA_WEB_ALLOW_KEY_EDIT not set");
    }

    let cap = state.config.describer.passes_cap;
    let first = body.passes_first.min(cap).max(1);
    let refresh = body.passes_refresh.min(cap).max(1);

    let cfg_path = config::default_config_path();
    // A missing file loads as Config::default(); an Err here means the file EXISTS but
    // failed to parse — never overwrite (and silently wipe [api_keys]) a malformed config
    // the user can still fix by hand. (Previously `.unwrap_or_default()` clobbered it.)
    let mut cfg = match config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config exists but failed to parse; refusing to overwrite it: {e:#}"),
            )
        }
    };
    cfg.describer.passes_first = first;
    cfg.describer.passes_refresh = refresh;

    match config::save(&cfg, &cfg_path) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Read the current resource/workload profile from the in-memory startup config.
/// Like `api_config_get`, this reflects the snapshot loaded at `indexa serve`
/// startup, so a value just POSTed via `api_config_resource_set` won't appear
/// here until the next launch — that's fine: the profile "applies to the next job".
pub(crate) async fn api_config_resource_get(
    State(state): State<AppState>,
) -> Json<ResourceResponse> {
    let res = &state.config.resource;
    Json(ResourceResponse {
        profile: res.profile.as_str().to_owned(),
        headroom_gb: res.headroom_gb,
    })
}

/// Persist the resource/workload profile (RAM headroom + how hard Indexa pushes
/// the machine) to config.toml.
///
/// # Deliberately ungated (no `INDEXA_WEB_ALLOW_KEY_EDIT` check)
/// Unlike `api_config_passes`/`api_keys`, this endpoint is intentionally NOT gated
/// behind `INDEXA_WEB_ALLOW_KEY_EDIT=1`. Rationale:
/// - The resource profile is **not a secret** — it only governs memory headroom and
///   throughput aggressiveness, nothing security-sensitive.
/// - Forcing a relaunch-with-env-var just to dial one's own workload *down* (e.g.
///   when the machine is under load) defeats the entire purpose of the control.
/// - `ResourceRequest` carries ONLY `profile` + `headroom_gb` — there are no key
///   fields to inject, so an unauthenticated caller cannot write secrets here.
/// - This handler mutates ONLY `cfg.resource.profile` and `cfg.resource.headroom_gb`.
///   Every other section (`[api_keys]`, `[describer]`, …) is preserved verbatim by
///   the `config::load` → mutate → `config::save` round-trip; we never read or write
///   key material.
/// - CORS is already locked to the localhost origin (see `serve()` in lib.rs), so the
///   worst case for an unguarded call is flipping a non-secret workload profile.
pub(crate) async fn api_config_resource_set(Json(body): Json<ResourceRequest>) -> Response {
    // String → enum; unknown values fall back to the safe Balanced default.
    let profile = match body.profile.as_str() {
        "conservative" => ResourceProfile::Conservative,
        "performance" => ResourceProfile::Performance,
        _ => ResourceProfile::Balanced,
    };

    let cfg_path = config::default_config_path();
    // A missing file loads as Config::default(); an Err here means the file EXISTS but
    // failed to parse — never overwrite (and silently wipe [api_keys]) a malformed config
    // the user can still fix by hand. (Previously `.unwrap_or_default()` clobbered it.)
    let mut cfg = match config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config exists but failed to parse; refusing to overwrite it: {e:#}"),
            )
        }
    };
    // Scope the write strictly to the two resource fields — see the ungated rationale above.
    cfg.resource.profile = profile;
    // Reject non-finite input (NaN / ±inf) → 0.0 ("use the profile's built-in headroom"); clamp
    // finite values to [0, 4096] GB. An unbounded headroom would saturate
    // effective_headroom_bytes() to u64::MAX and wedge the watchdog (no free RAM would ever suffice).
    cfg.resource.headroom_gb = if body.headroom_gb.is_finite() {
        body.headroom_gb.clamp(0.0, 4096.0)
    } else {
        0.0
    };

    match config::save(&cfg, &cfg_path) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Return the current advanced feature toggle states (ANN, image caption, audio transcription).
pub(crate) async fn api_config_features_get(
    State(state): State<AppState>,
) -> Json<FeaturesResponse> {
    Json(FeaturesResponse {
        ann: state.config.retrieval.ann,
        ann_min_chunks: state.config.retrieval.ann_min_chunks,
        image_caption: state.config.parsers.image.caption,
        image_model: state.config.parsers.image.model.clone(),
        audio_transcribe: state.config.parsers.audio.transcribe,
        audio_binary: state.config.parsers.audio.binary.clone(),
        video_caption: state.config.parsers.video.caption,
        video_model: state.config.parsers.video.model.clone(),
    })
}

/// Persist advanced feature toggles. Ungated — no secrets involved. Only supplied
/// fields are written; every other config section is preserved by the round-trip.
pub(crate) async fn api_config_features_set(Json(body): Json<FeaturesRequest>) -> Response {
    let cfg_path = config::default_config_path();
    let mut cfg = match config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config exists but failed to parse; refusing to overwrite it: {e:#}"),
            )
        }
    };
    if let Some(v) = body.ann {
        cfg.retrieval.ann = v;
    }
    if let Some(v) = body.ann_min_chunks {
        cfg.retrieval.ann_min_chunks = v;
    }
    if let Some(v) = body.image_caption {
        cfg.parsers.image.caption = v;
    }
    if let Some(v) = body.image_model {
        cfg.parsers.image.model = if v.trim().is_empty() {
            None
        } else {
            Some(v.trim().to_owned())
        };
    }
    if let Some(v) = body.audio_transcribe {
        cfg.parsers.audio.transcribe = v;
    }
    if let Some(v) = body.audio_binary {
        cfg.parsers.audio.binary = if v.trim().is_empty() {
            None
        } else {
            Some(v.trim().to_owned())
        };
    }
    if let Some(v) = body.video_caption {
        cfg.parsers.video.caption = v;
    }
    if let Some(v) = body.video_model {
        cfg.parsers.video.model = if v.trim().is_empty() {
            None
        } else {
            Some(v.trim().to_owned())
        };
    }
    match config::save(&cfg, &cfg_path) {
        Ok(_) => {
            Json(serde_json::json!({ "saved": true, "restart_required": true })).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
