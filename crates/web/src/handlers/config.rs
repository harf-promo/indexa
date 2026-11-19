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

pub(crate) async fn api_config_get(State(state): State<AppState>) -> Response {
    let current = match config::load(&state.config_path) {
        Ok(config) => config,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let cfg = &current.describer;
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
        embed_model: current.embedding.model.clone(),
    })
    .into_response()
}

/// Persist describer/embedding model assignments (provider, per-role models,
/// base URL) to config.toml. Gated like `api_config_passes` — these select which
/// model runs and can point at a remote endpoint, so they are not ungated like the
/// non-secret resource profile. Only the fields present in the body are written;
/// every other config section is preserved by the load → mutate → save round-trip.
pub(crate) async fn api_config_provider_set(
    State(state): State<AppState>,
    Json(body): Json<ProviderRequest>,
) -> Response {
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

    let cfg_path = &*state.config_path;
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
    match config::update(cfg_path, |cfg| {
        if let Some(v) = body.provider {
            cfg.describer.provider = v.trim().to_owned();
        }
        set(&mut cfg.describer.model, body.model);
        set(&mut cfg.describer.file_model, body.file_model);
        set(&mut cfg.describer.dir_model, body.dir_model);
        set(&mut cfg.describer.base_url, body.base_url);
        set(&mut cfg.embedding.model, body.embed_model);
        Ok(())
    }) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
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

    let cfg_path = &*state.config_path;
    // A missing file loads as Config::default(); an Err here means the file EXISTS but
    // failed to parse — never overwrite (and silently wipe [api_keys]) a malformed config
    // the user can still fix by hand. (Previously `.unwrap_or_default()` clobbered it.)
    match config::update(cfg_path, |cfg| {
        let cap = cfg.describer.passes_cap;
        cfg.describer.passes_first = body.passes_first.min(cap).max(1);
        cfg.describer.passes_refresh = body.passes_refresh.min(cap).max(1);
        Ok(())
    }) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Read the current resource/workload profile from config.toml.
pub(crate) async fn api_config_resource_get(State(state): State<AppState>) -> Response {
    let current = match config::load(&state.config_path) {
        Ok(config) => config,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let res = &current.resource;
    Json(ResourceResponse {
        profile: res.profile.as_str().to_owned(),
        headroom_gb: res.headroom_gb,
    })
    .into_response()
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
pub(crate) async fn api_config_resource_set(
    State(state): State<AppState>,
    Json(body): Json<ResourceRequest>,
) -> Response {
    // String → enum; unknown values fall back to the safe Balanced default.
    let profile = match body.profile.as_str() {
        "conservative" => ResourceProfile::Conservative,
        "performance" => ResourceProfile::Performance,
        _ => ResourceProfile::Balanced,
    };

    let cfg_path = &*state.config_path;
    // A missing file loads as Config::default(); an Err here means the file EXISTS but
    // failed to parse — never overwrite (and silently wipe [api_keys]) a malformed config
    // the user can still fix by hand. (Previously `.unwrap_or_default()` clobbered it.)
    match config::update(cfg_path, |cfg| {
        cfg.resource.profile = profile;
        cfg.resource.headroom_gb = if body.headroom_gb.is_finite() {
            body.headroom_gb.clamp(0.0, 4096.0)
        } else {
            0.0
        };
        Ok(())
    }) {
        Ok(_) => Json(serde_json::json!({ "saved": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Return the current advanced feature toggle states (ANN, image caption, audio transcription).
pub(crate) async fn api_config_features_get(State(state): State<AppState>) -> Response {
    let cfg = match config::load(&state.config_path) {
        Ok(config) => config,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    Json(FeaturesResponse {
        ann: cfg.retrieval.ann,
        ann_min_chunks: cfg.retrieval.ann_min_chunks,
        image_caption: cfg.parsers.image.caption,
        image_model: cfg.parsers.image.model.clone(),
        audio_transcribe: cfg.parsers.audio.transcribe,
        audio_binary: cfg.parsers.audio.binary.clone(),
        video_caption: cfg.parsers.video.caption,
        video_model: cfg.parsers.video.model.clone(),
        video_transcribe: cfg.parsers.video.transcribe,
    })
    .into_response()
}

#[derive(Debug)]
struct FeatureWriteDenied;
impl std::fmt::Display for FeatureWriteDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("INDEXA_WEB_ALLOW_KEY_EDIT not set")
    }
}
impl std::error::Error for FeatureWriteDenied {}

/// Honest multimodal memory check (v0.38): when image/video captioning is enabled the
/// vision model runs **co-resident** with the summarization models during `deep`, so the
/// real peak is the {file, dir, caption} trio. Returns a calm warning — we don't block
/// (local-first; the user owns the machine) — when the trio overflows the budget or the
/// caption model's footprint is unknown. Audio transcription is excluded: it shells to an
/// external whisper-cli process, not an Ollama model, so it's not in this budget.
fn caption_budget_warning(
    cfg: &config::Config,
    spec: &indexa_core::resource::MachineSpec,
) -> Option<String> {
    let model = if cfg.parsers.image.caption {
        cfg.parsers.image.caption_model().to_owned()
    } else if cfg.parsers.video.caption {
        cfg.parsers.video.caption_model().to_owned()
    } else {
        return None;
    };
    let sample = indexa_core::resource::sample_memory_once();
    let cf = indexa_core::resource::caption_fit_report(
        &cfg.describer.file_model,
        &cfg.describer.dir_model,
        &model,
        cfg.describer.num_ctx,
        spec,
        &sample,
        cfg.resource.effective_headroom_bytes(),
    );
    const GB: f64 = (1024 * 1024 * 1024) as f64;
    if !cf.caption_model_known {
        return Some(format!(
            "Captioning with \u{201c}{model}\u{201d} is on, but its memory footprint is unknown — the watchdog can't size it. Enable only with headroom."
        ));
    }
    if cf.fits {
        return None;
    }
    let base = format!(
        "Captioning with \u{201c}{model}\u{201d} needs ~{:.1} GB alongside the summary models, but only ~{:.1} GB is budgeted above the keep-free band.",
        cf.trio_peak_bytes as f64 / GB,
        cf.budget_bytes.max(0) as f64 / GB,
    );
    Some(match cf.lighter_suggestion {
        Some(l) => format!(
            "{base} Try a lighter model like \u{201c}{l}\u{201d}. It's saved — captioning may pause under memory pressure."
        ),
        None => format!("{base} It's saved — captioning may pause under memory pressure."),
    })
}

/// Persist advanced feature toggles. Mostly ungated — no secrets involved for
/// ann/image/video captioning or the audio-transcribe toggle itself. `audio_binary` is
/// the one exception (H7): unlike `pdf.ocr_binary`/`video.ffmpeg_binary`, which have no
/// web setter at all, this field names an executable path the indexer later spawns
/// (`Command::new(binary)`, fixed args, no shell — but still program selection, not
/// just endpoint selection). Gated behind `INDEXA_WEB_ALLOW_KEY_EDIT=1` like
/// `api_config_provider_set`, so a LAN-shared token alone can't repoint it.
///
/// Gated on CHANGE, not presence: the Features panel always resends every field on
/// Save (including the unmodified current `audio_binary`), so gating on
/// `body.audio_binary.is_some()` alone would lock a caller out of every other,
/// non-sensitive toggle the moment a binary path was ever configured — a no-op resend
/// of the already-stored value is not a repoint attempt and must not require the gate.
///
/// The "changed" comparison is against the freshly-`config::load`ed value below, NOT
/// `state.config` — `AppState.config` is a snapshot taken once at server startup and is
/// never refreshed, so comparing against it would let an ungated caller "restore" a
/// value that was merely present at boot (stale-snapshot bypass), silently reverting a
/// legitimate gated change made since startup. Comparing against the value this same
/// request just loaded from disk is always current by construction. Only supplied
/// fields are written; every other config section is preserved by the round-trip.
pub(crate) async fn api_config_features_set(
    State(state): State<AppState>,
    Json(body): Json<FeaturesRequest>,
) -> Response {
    let cfg_path = &*state.config_path;
    match config::update(cfg_path, |cfg| {
        let audio_binary_changed =
            matches!(&body.audio_binary, Some(v) if Some(v) != cfg.parsers.audio.binary.as_ref());
        if audio_binary_changed && std::env::var("INDEXA_WEB_ALLOW_KEY_EDIT").as_deref() != Ok("1")
        {
            return Err(anyhow::Error::new(FeatureWriteDenied));
        }
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
        if let Some(v) = body.video_transcribe {
            cfg.parsers.video.transcribe = v;
        }
        if let Some(v) = body.video_model {
            cfg.parsers.video.model = if v.trim().is_empty() {
                None
            } else {
                Some(v.trim().to_owned())
            };
        }
        Ok(())
    }) {
        Ok(cfg) => {
            let caption_warning = caption_budget_warning(&cfg, &state.machine_spec);
            Json(serde_json::json!({
                "saved": true,
                "caption_warning": caption_warning,
            }))
            .into_response()
        }
        Err(e) if e.downcast_ref::<FeatureWriteDenied>().is_some() => {
            err_json(StatusCode::FORBIDDEN, "INDEXA_WEB_ALLOW_KEY_EDIT not set")
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
