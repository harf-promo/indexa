use axum::{
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use crate::dto::{err_json, ModelInfo, ModelRow, ModelsQuery, ModelsResponse, PullRequest};
use crate::AppState;
use indexa_core::models_catalog::{bundled_catalog, CatalogModel, ModelRole};
use indexa_core::resource::{compute_budget, estimate_eta_with, footprint_for, sample_memory_once};

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

// ── B2: installed-model metadata (Ollama /api/tags + /api/show) ──────────────

/// A reqwest client with bounded timeouts, so a stalled Ollama or catalog host
/// can't hang the `/api/models` handler indefinitely. Used for the non-streaming
/// JSON calls only (the streaming `/api/pull` proxy must stay un-timed).
fn json_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// An installed Ollama model with its real on-disk size and best-effort metadata.
struct InstalledModel {
    name: String,
    /// Real on-disk weights size from `/api/tags`.
    size: u64,
    /// `details.parameter_size` from `/api/show`, parsed to billions.
    params_b: Option<f64>,
    /// `details.quantization_level` from `/api/show`.
    quant: Option<String>,
}

/// Fetch installed models from Ollama `/api/tags`, then enrich each with
/// `/api/show` metadata (best-effort, serial — fine for the handful of installed
/// models). `/api/show` reads the manifest without loading the model (token-free).
async fn fetch_installed(base: &str) -> Result<Vec<InstalledModel>, String> {
    let client = json_client();
    let tags: serde_json::Value = client
        .get(format!("{base}/api/tags"))
        .send()
        .await
        .map_err(|e| format!("{e:#}"))?
        .json()
        .await
        .map_err(|e| format!("{e:#}"))?;
    let mut out = Vec::new();
    if let Some(arr) = tags["models"].as_array() {
        for m in arr {
            let name = m["name"].as_str().unwrap_or("").to_owned();
            if name.is_empty() {
                continue;
            }
            let size = m["size"].as_u64().unwrap_or(0);
            let (params_b, quant) = show_metadata(&client, base, &name).await;
            out.push(InstalledModel {
                name,
                size,
                params_b,
                quant,
            });
        }
    }
    Ok(out)
}

/// Best-effort `/api/show` lookup → (params_b, quant). Any error → (None, None).
async fn show_metadata(
    client: &reqwest::Client,
    base: &str,
    name: &str,
) -> (Option<f64>, Option<String>) {
    let Ok(resp) = client
        .post(format!("{base}/api/show"))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
    else {
        return (None, None);
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return (None, None);
    };
    let details = &body["details"];
    let params_b = details["parameter_size"].as_str().and_then(parse_params_b);
    let quant = details["quantization_level"].as_str().map(str::to_owned);
    (params_b, quant)
}

/// Parse an Ollama `parameter_size` string (`"12.2B"`, `"137M"`) to billions.
fn parse_params_b(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, scale) = if let Some(n) = s.strip_suffix(['B', 'b']) {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 0.001)
    } else {
        (s, 1.0)
    };
    num.trim().parse::<f64>().ok().map(|v| v * scale)
}

// ── B3 (web side): catalog overlay + fail-open online refresh ────────────────

/// In-memory overlay populated by the online refresh. `None` → serve the bundled
/// catalog. Kept here (not in core) so core stays pure / unit-testable.
static CATALOG_OVERLAY: RwLock<Option<Vec<CatalogModel>>> = RwLock::new(None);

/// The catalog to serve: the refreshed overlay if present, else the bundled one.
fn effective_catalog() -> Vec<CatalogModel> {
    if let Ok(guard) = CATALOG_OVERLAY.read() {
        if let Some(cat) = guard.as_ref() {
            return cat.clone();
        }
    }
    bundled_catalog()
}

/// `POST /api/models/catalog/refresh` — best-effort fetch of a configured JSON
/// catalog URL, merged over the bundled catalog. **Fails open**: no URL → no-op;
/// any fetch/parse error → the bundled/prior catalog stays in place.
pub(crate) async fn api_models_catalog_refresh(State(s): State<AppState>) -> Response {
    let Some(url) = s.config.models.catalog_url.clone() else {
        return Json(serde_json::json!({
            "refreshed": false,
            "reason": "no catalog_url configured",
            "count": bundled_catalog().len(),
        }))
        .into_response();
    };
    match fetch_catalog(&url).await {
        Ok(fetched) => {
            let merged = merge_catalog(bundled_catalog(), fetched);
            let count = merged.len();
            if let Ok(mut guard) = CATALOG_OVERLAY.write() {
                *guard = Some(merged);
            }
            Json(serde_json::json!({ "refreshed": true, "source": url, "count": count }))
                .into_response()
        }
        Err(e) => Json(serde_json::json!({ "refreshed": false, "error": e })).into_response(),
    }
}

async fn fetch_catalog(url: &str) -> Result<Vec<CatalogModel>, String> {
    let resp = json_client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{e:#}"))?;
    let models: Vec<CatalogModel> = resp.json().await.map_err(|e| format!("{e:#}"))?;
    Ok(models.into_iter().filter(|m| !m.name.is_empty()).collect())
}

/// Merge a fetched catalog over the bundled one — fetched entries win on a
/// name collision (normalized), new names are appended.
fn merge_catalog(mut bundled: Vec<CatalogModel>, fetched: Vec<CatalogModel>) -> Vec<CatalogModel> {
    for f in fetched {
        match bundled
            .iter_mut()
            .find(|b| normalize_name(&b.name) == normalize_name(&f.name))
        {
            Some(existing) => *existing = f,
            None => bundled.push(f),
        }
    }
    bundled
}

// ── B4: unified GET /api/models (installed ∪ catalog, each with fit + ETA) ────

/// Dedup key: a bare name and its `:latest` variant are the same model.
fn normalize_name(name: &str) -> String {
    name.strip_suffix(":latest").unwrap_or(name).to_owned()
}

fn role_str(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Embed => "embed",
        ModelRole::File => "file",
        ModelRole::Dir => "dir",
        ModelRole::Qa => "qa",
        ModelRole::Code => "code",
        ModelRole::Vision => "vision",
    }
}

/// Cheap heuristic to tag an installed-but-uncatalogued model as an embedder, so
/// its ETA uses the embed (chunk) workload rather than the generate (file) one.
fn looks_like_embedder(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("embed") || n.contains("bge") || n.contains("minilm")
}

/// A unified model spec before it is costed into a [`ModelRow`].
struct Candidate {
    name: String,
    role: ModelRole,
    vendor: String,
    params_b: Option<f64>,
    quant: Option<String>,
    /// Real on-disk size when installed; `None` for catalog-only models.
    installed_size: Option<u64>,
    recommended_default: bool,
    safe_default: bool,
}

/// The shared costing context (budget + workload + machine) for a request.
struct RowCtx {
    num_ctx: u32,
    budget: i64,
    n_files: usize,
    n_chunks: usize,
    passes: u32,
    is_apple_m3: bool,
    embed_per_min: f64,
}

impl RowCtx {
    fn build(&self, c: &Candidate) -> ModelRow {
        let installed = c.installed_size.is_some();
        let mut fp = footprint_for(&c.name, c.params_b, c.quant.as_deref(), self.num_ctx);
        // Installed models: trust the real on-disk weights over any estimate.
        if let Some(sz) = c.installed_size {
            fp.weights_bytes = sz;
        }
        let peak_bytes = fp.peak_bytes(self.num_ctx);
        let fits = (peak_bytes as i64) <= self.budget;
        // ETA split by role: never fold embed time into a summarizer (or vice versa).
        let eta = if c.role == ModelRole::Embed {
            estimate_eta_with(
                &fp,
                0,
                self.n_chunks,
                600,
                1,
                self.is_apple_m3,
                self.embed_per_min,
            )
        } else {
            estimate_eta_with(
                &fp,
                self.n_files,
                0,
                600,
                self.passes,
                self.is_apple_m3,
                self.embed_per_min,
            )
        };
        let size_bytes = c.installed_size.unwrap_or(fp.weights_bytes);
        ModelRow {
            name: c.name.clone(),
            role: role_str(c.role).to_owned(),
            vendor: c.vendor.clone(),
            params_b: c.params_b,
            size_bytes,
            size_is_estimate: !installed,
            installed,
            peak_bytes,
            fits,
            eta_display: eta.display,
            eta_secs: eta.total_secs as u64,
            recommended_default: c.recommended_default,
            safe_default: c.safe_default,
        }
    }
}

/// `GET /api/models?num_ctx=…` — every model (installed ∪ catalog), each with its
/// real/estimated size, whether it fits the live memory budget, and an ETA for
/// the current workload. Powers PR-C's Local-vs-Cloud picker.
///
/// Counts are global; `?path=` subtree scoping is deferred to PR-C (mirrors
/// `GET /api/jobs/estimate`'s existing behavior).
pub(crate) async fn api_models(
    Query(q): Query<ModelsQuery>,
    State(s): State<AppState>,
) -> Response {
    let cfg = &s.config.describer;
    let num_ctx = q.num_ctx.unwrap_or(cfg.num_ctx);
    let headroom = s.config.resource.effective_headroom_bytes();
    let budget = compute_budget(&s.machine_spec, &sample_memory_once(), headroom);
    let is_apple_m3 = s.machine_spec.is_apple_silicon;
    let embed_per_min = if is_apple_m3 { 400.0 } else { 120.0 };

    // Workload size for the ETA. Global counts; a representative ~200-file job
    // when the index is empty, so a "no job selected" estimate always exists.
    let (mut n_files, mut n_chunks) = {
        let store = s.store.lock().await;
        (
            store.entry_count().unwrap_or(0) as usize,
            store.chunk_count().unwrap_or(0) as usize,
        )
    };
    if n_files == 0 && n_chunks == 0 {
        n_files = 200;
        n_chunks = 600;
    }

    let ctx = RowCtx {
        num_ctx,
        budget,
        n_files,
        n_chunks,
        passes: cfg.passes_first.max(1),
        is_apple_m3,
        embed_per_min,
    };

    // Installed models, keyed by normalized name (best-effort: an Ollama outage
    // simply yields catalog-only rows).
    let installed_by_key: HashMap<String, InstalledModel> = fetch_installed(&cfg.base_url)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| (normalize_name(&m.name), m))
        .collect();

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Catalog first — it carries role/vendor/flags; enrich with installed facts.
    for cm in effective_catalog() {
        let key = normalize_name(&cm.name);
        seen.insert(key.clone());
        let inst = installed_by_key.get(&key);
        candidates.push(Candidate {
            name: cm.name.clone(),
            role: cm.role,
            vendor: cm.vendor.clone(),
            // Prefer real /api/show metadata when installed, else the catalog's.
            params_b: inst.and_then(|m| m.params_b).or(Some(cm.params_b)),
            quant: inst
                .and_then(|m| m.quant.clone())
                .or_else(|| Some(cm.quant.clone())),
            installed_size: inst.map(|m| m.size),
            recommended_default: cm.recommended_default,
            safe_default: cm.safe_default,
        });
    }

    // Installed-but-uncatalogued models.
    for (key, m) in &installed_by_key {
        if seen.contains(key) {
            continue;
        }
        let is_embed = looks_like_embedder(&m.name);
        candidates.push(Candidate {
            name: m.name.clone(),
            role: if is_embed {
                ModelRole::Embed
            } else {
                ModelRole::Qa
            },
            vendor: String::new(),
            params_b: m.params_b,
            quant: m.quant.clone(),
            installed_size: Some(m.size),
            recommended_default: false,
            safe_default: true,
        });
    }

    let mut models: Vec<ModelRow> = candidates.iter().map(|c| ctx.build(c)).collect();
    models.sort_by(|a, b| a.name.cmp(&b.name));

    Json(ModelsResponse {
        budget_bytes: budget,
        num_ctx,
        models,
    })
    .into_response()
}
