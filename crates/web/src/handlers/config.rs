use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::config;
use indexa_core::resource::ResourceProfile;

use crate::dto::{err_json, ConfigResponse, PassesRequest, ResourceRequest, ResourceResponse};
use crate::AppState;

pub(crate) async fn api_config_get(State(state): State<AppState>) -> Json<ConfigResponse> {
    let cfg = &state.config.describer;
    Json(ConfigResponse {
        passes_first: cfg.passes_first,
        passes_refresh: cfg.passes_refresh,
        passes_cap: cfg.passes_cap,
        max_children_per_summary: cfg.max_children_per_summary,
    })
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
    let mut cfg = config::load(&cfg_path).unwrap_or_default();
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
    let mut cfg = config::load(&cfg_path).unwrap_or_default();
    // Scope the write strictly to the two resource fields — see the ungated rationale above.
    cfg.resource.profile = profile;
    cfg.resource.headroom_gb = body.headroom_gb.max(0.0);

    match config::save(&cfg, &cfg_path) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
