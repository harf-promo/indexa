//! Resource detection, memory budgeting, and model-fit logic.
//!
//! This module answers three questions before each Ollama job:
//!   1. How much memory does this machine have, and how much is free right now?
//!   2. Do the models we want to run fit within the available headroom?
//!   3. Are we currently under memory pressure that should throttle the next call?
//!
//! # Apple Silicon note
//! On Apple Silicon, CPU and GPU share one unified memory pool.  The Metal
//! driver can wire at most ~75 % of total RAM for GPU use, so the effective
//! budget is `min(free_ram, 0.75 * total_ram) - headroom`.  This is `cfg`-gated;
//! non-Apple platforms use plain available-RAM headroom.
//!
//! # macOS memory-signal caveat
//! `sysinfo::System::available_memory()` on macOS returns 0, and `free_memory`
//! is near-permanently low because the OS fills free RAM with reclaimable file
//! cache. The reliable signal is `compute_budget` = `total − used_bytes − headroom`
//! (`used_bytes` = active + wired pages, which excludes that cache), so `assess()`
//! judges pressure from the *budget*, not from swap fraction — which on macOS is
//! sticky (grows dynamically, never drains) and produced false positives.

use sysinfo::System;

// ── Machine spec ──────────────────────────────────────────────────────────────

/// Static description of the host machine, detected once at startup.
#[derive(Debug, Clone)]
pub struct MachineSpec {
    /// Total physical RAM in bytes.
    pub total_ram_bytes: u64,
    /// Physical (non-hyper-threaded) CPU cores.
    pub physical_cores: usize,
    /// Logical CPU threads (what `available_parallelism` returns).
    pub logical_cores: usize,
    /// True on macOS + aarch64 (Apple Silicon unified memory).
    pub is_apple_silicon: bool,
    /// Effective GPU-wired memory ceiling in bytes.
    /// Apple Silicon: ~75 % of total RAM.  Others: equal to total_ram_bytes.
    pub gpu_wired_limit_bytes: u64,
}

impl MachineSpec {
    /// Human-readable RAM string, e.g. "36 GB".
    pub fn ram_display(&self) -> String {
        format!("{} GB", self.total_ram_bytes / (1024 * 1024 * 1024))
    }
}

/// Detect the host machine spec.  Cheap — call once and store the result.
pub fn detect_machine() -> MachineSpec {
    let mut sys = System::new();
    sys.refresh_memory();

    let total_ram_bytes = sys.total_memory();

    let physical_cores = sys.physical_core_count().unwrap_or(1);
    let logical_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(physical_cores);

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let is_apple_silicon = true;
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    let is_apple_silicon = false;

    // Apple Silicon: Metal can wire at most ~75 % of unified RAM.
    let gpu_wired_limit_bytes = if is_apple_silicon {
        (total_ram_bytes as f64 * 0.75) as u64
    } else {
        total_ram_bytes
    };

    MachineSpec {
        total_ram_bytes,
        physical_cores,
        logical_cores,
        is_apple_silicon,
        gpu_wired_limit_bytes,
    }
}

// ── Live memory sample ────────────────────────────────────────────────────────

/// A cheap snapshot of current memory state.
///
/// Obtain by calling `sample_memory(&mut sys)` where `sys` is a long-lived
/// `sysinfo::System`.  Only `refresh_memory()` is called — avoid the much
/// more expensive `refresh_cpu()` in hot loops.
#[derive(Debug, Clone, Default)]
pub struct MemSample {
    /// Bytes reported as "available" by the OS (unreliable on macOS — see module docs).
    pub available_bytes: u64,
    /// Bytes reported as "used" (includes file cache on macOS).
    pub used_bytes: u64,
    /// Bytes of swap currently in use.  Rising swap is the key freeze signal.
    pub swap_used_bytes: u64,
    /// Bytes of swap capacity.
    pub swap_total_bytes: u64,
    /// Bytes truly free (not available — free pages only).
    pub free_bytes: u64,
}

/// Refresh memory state and return a `MemSample`.
///
/// Pass in a long-lived `sysinfo::System` instance to avoid re-constructing it.
pub fn sample_memory(sys: &mut System) -> MemSample {
    sys.refresh_memory();
    MemSample {
        available_bytes: sys.available_memory(),
        used_bytes: sys.used_memory(),
        swap_used_bytes: sys.used_swap(),
        swap_total_bytes: sys.total_swap(),
        free_bytes: sys.free_memory(),
    }
}

/// Convenience: create a throwaway `System`, sample once, and return the result.
/// Use this only for one-off checks (e.g. `indexa doctor`) — for hot paths,
/// hold a long-lived `System` and call `sample_memory` instead.
pub fn sample_memory_once() -> MemSample {
    let mut sys = System::new();
    sample_memory(&mut sys)
}

/// Opaque watchdog state that encapsulates the long-lived `sysinfo::System`.
///
/// Create one at the start of a job and call `sample()` before each Ollama
/// request.  This avoids callers needing a direct dependency on `sysinfo`.
pub struct WatchdogState(System);

impl WatchdogState {
    pub fn new() -> Self {
        Self(System::new())
    }

    /// Sample current memory state.
    pub fn sample(&mut self) -> MemSample {
        sample_memory(&mut self.0)
    }
}

impl Default for WatchdogState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Live CPU sample + combined telemetry sampler ──────────────────────────────

/// A snapshot of CPU utilisation, 0–100 % across all logical cores.
///
/// CPU usage is a *delta* measurement: `sysinfo` computes it from the difference
/// between two refreshes, so the very first refresh after construction yields no
/// meaningful value (see [`TelemetrySampler`], which discards it).
#[derive(Debug, Clone, Default)]
pub struct CpuSample {
    /// System-wide CPU usage, 0–100 %.
    pub global_percent: f32,
    /// Per-logical-core usage, 0–100 %.
    pub per_core: Vec<f32>,
}

/// A long-lived sampler for the always-on machine telemetry feed (CPU + memory).
///
/// Unlike [`WatchdogState`] — which deliberately samples *memory only* to stay
/// cheap inside the per-file job hot loop — this sampler also calls the more
/// expensive `refresh_cpu()`. It is meant to run on its **own** low-frequency
/// task (~1–2 s cadence), never in a hot loop, so the cost is negligible.
///
/// Because CPU usage needs two refreshes spaced apart, the first call to
/// [`sample`](Self::sample) returns `cpu = None` (priming); every subsequent call
/// returns a real reading.
pub struct TelemetrySampler {
    sys: System,
    primed: bool,
}

impl TelemetrySampler {
    pub fn new() -> Self {
        Self {
            sys: System::new(),
            primed: false,
        }
    }

    /// Refresh CPU + memory and return `(cpu, mem)`. `cpu` is `None` on the first
    /// call (priming the delta) and `Some` thereafter.
    pub fn sample(&mut self) -> (Option<CpuSample>, MemSample) {
        self.sys.refresh_cpu();
        let cpu = if self.primed {
            Some(CpuSample {
                global_percent: self.sys.global_cpu_info().cpu_usage(),
                per_core: self.sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
            })
        } else {
            self.primed = true;
            None
        };
        let mem = sample_memory(&mut self.sys);
        (cpu, mem)
    }
}

impl Default for TelemetrySampler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Model footprint table ─────────────────────────────────────────────────────

/// Memory footprint estimate for a single Ollama model.
#[derive(Debug, Clone)]
pub struct ModelFootprint {
    /// Ollama model name (as it appears in `ollama list`).
    pub name: &'static str,
    /// Resident weights in bytes (Q4_K_M quantisation, rounded up to be safe).
    pub weights_bytes: u64,
    /// KV-cache bytes per context token, per parallel slot.
    pub kv_bytes_per_ctx_token: u64,
    /// Default context window tokens used when estimating peak memory.
    pub default_num_ctx: u32,
    /// Approximate prompt-evaluation throughput on Apple M3 Max (tokens/sec).
    /// Used for ETA estimation.  0 means unknown.
    pub prompt_eval_tok_s_apple_m3: f64,
    /// Approximate generation throughput on Apple M3 Max (tokens/sec).
    pub gen_tok_s_apple_m3: f64,
    /// Fallback throughput estimate for non-M3 machines (conservative).
    pub gen_tok_s_generic: f64,
}

impl ModelFootprint {
    /// Peak memory in bytes for one concurrent request.
    ///
    /// `num_parallel = 1` is always assumed (Indexa pins this via request options).
    pub fn peak_bytes(&self, num_ctx: u32) -> u64 {
        self.weights_bytes + self.kv_bytes_per_ctx_token * u64::from(num_ctx)
    }

    /// Human-readable peak estimate string.
    pub fn peak_display(&self, num_ctx: u32) -> String {
        let gb = self.peak_bytes(num_ctx) as f64 / (1024.0 * 1024.0 * 1024.0);
        format!("{gb:.1} GB")
    }
}

/// Seed table of known model footprints.
///
/// Add entries here as new models are tested.  All values are rounded **up**
/// so the budget errs on the side of safety.
pub static MODEL_FOOTPRINTS: &[ModelFootprint] = &[
    ModelFootprint {
        name: "nomic-embed-text",
        weights_bytes: 350 * 1024 * 1024, // ~0.35 GB
        kv_bytes_per_ctx_token: 512,      // tiny encoder, negligible KV
        default_num_ctx: 2048,
        prompt_eval_tok_s_apple_m3: 2000.0, // embedding is fast
        gen_tok_s_apple_m3: 2000.0,
        gen_tok_s_generic: 500.0,
    },
    ModelFootprint {
        name: "nomic-embed-text:latest",
        weights_bytes: 350 * 1024 * 1024,
        kv_bytes_per_ctx_token: 512,
        default_num_ctx: 2048,
        prompt_eval_tok_s_apple_m3: 2000.0,
        gen_tok_s_apple_m3: 2000.0,
        gen_tok_s_generic: 500.0,
    },
    ModelFootprint {
        name: "gemma3:4b",
        weights_bytes: 4 * 1024 * 1024 * 1024, // ~4 GB (Q4_K_M)
        kv_bytes_per_ctx_token: 2048,          // ~2 KB/token
        default_num_ctx: 4096,
        prompt_eval_tok_s_apple_m3: 800.0,
        gen_tok_s_apple_m3: 75.0,
        gen_tok_s_generic: 20.0,
    },
    ModelFootprint {
        name: "gemma3:12b",
        weights_bytes: 9 * 1024 * 1024 * 1024, // ~9 GB (Q4_K_M)
        kv_bytes_per_ctx_token: 4096,          // ~4 KB/token
        default_num_ctx: 4096,
        prompt_eval_tok_s_apple_m3: 400.0,
        gen_tok_s_apple_m3: 30.0,
        gen_tok_s_generic: 8.0,
    },
    ModelFootprint {
        name: "gemma2:9b",
        weights_bytes: 6 * 1024 * 1024 * 1024,
        kv_bytes_per_ctx_token: 3072,
        default_num_ctx: 4096,
        prompt_eval_tok_s_apple_m3: 500.0,
        gen_tok_s_apple_m3: 45.0,
        gen_tok_s_generic: 12.0,
    },
    ModelFootprint {
        name: "mistral-small:22b",
        weights_bytes: 13 * 1024 * 1024 * 1024,
        kv_bytes_per_ctx_token: 6144,
        default_num_ctx: 4096,
        prompt_eval_tok_s_apple_m3: 250.0,
        gen_tok_s_apple_m3: 18.0,
        gen_tok_s_generic: 5.0,
    },
    ModelFootprint {
        name: "qwen2.5:14b",
        weights_bytes: 10 * 1024 * 1024 * 1024,
        kv_bytes_per_ctx_token: 4096,
        default_num_ctx: 4096,
        prompt_eval_tok_s_apple_m3: 350.0,
        gen_tok_s_apple_m3: 25.0,
        gen_tok_s_generic: 7.0,
    },
];

/// Look up a model by name, returning its footprint if known.
/// Returns `None` for unknown models (the caller should warn and use a
/// conservative default or skip the fit check).
pub fn lookup_footprint(name: &str) -> Option<&'static ModelFootprint> {
    MODEL_FOOTPRINTS.iter().find(|m| m.name == name)
}

/// Quantisation level → resident-weights multiplier in **GB per billion params**.
///
/// Calibrated so `params_b × quant_scale(quant)` lands near the measured Q4_K_M
/// weights in [`MODEL_FOOTPRINTS`] (e.g. 9B × 0.65 ≈ 5.9 GB ≈ the table's 6 GB).
/// `BF16`/`F16` are matched **before** the `Q*` prefixes because `BF16` begins
/// with `B`, not `F`. Unknown quants fall back to Q4_K_M, the common Ollama
/// default.
fn quant_scale(quant: &str) -> f64 {
    let q = quant.to_ascii_uppercase();
    if q.starts_with("BF16") || q.starts_with("F16") {
        2.0
    } else if q.starts_with("Q8") {
        1.15
    } else if q.starts_with("Q6") {
        0.95
    } else if q.starts_with("Q5") {
        0.80
    } else if q.starts_with("Q3") {
        0.47
    } else if q.starts_with("Q2") {
        0.35
    } else {
        // Q4_K_M / Q4_0 and any unknown quant → the common 4-bit default.
        0.65
    }
}

/// Derive a [`ModelFootprint`] purely from parameter count + quantisation.
///
/// For catalog / preview models that have no [`MODEL_FOOTPRINTS`] entry. The
/// constants are calibrated against that table (see [`quant_scale`]); the
/// estimate reads *lower* than the table for very small models, whose entries
/// are deliberately rounded up for safety. Good enough for a fit/ETA preview,
/// not a substitute for a measured entry on the hot path.
///
/// The synthesized `name` is the literal `"estimated"` (the field is
/// `&'static str`); the caller carries the real model name.
pub fn estimate_footprint(params_b: f64, quant: &str, ctx: u32) -> ModelFootprint {
    // Guard against zero/negative params so the divisions below stay finite.
    let params_b = params_b.max(0.1);
    let gib = 1024.0 * 1024.0 * 1024.0;
    ModelFootprint {
        name: "estimated",
        weights_bytes: (params_b * quant_scale(quant) * gib) as u64,
        kv_bytes_per_ctx_token: (params_b * 320.0) as u64,
        default_num_ctx: ctx.max(1),
        prompt_eval_tok_s_apple_m3: 4500.0 / params_b,
        gen_tok_s_apple_m3: 500.0 / params_b,
        gen_tok_s_generic: (500.0 / params_b) / 3.5,
    }
}

/// Resolve a footprint for **any** model name.
///
/// Resolution order, most accurate first:
/// 1. exact [`MODEL_FOOTPRINTS`] entry (measured) — `params_b`/`quant`/`ctx` are
///    ignored on a hit;
/// 2. [`estimate_footprint`] from `params_b` (e.g. Ollama `/api/show` metadata);
/// 3. a conservative unknown default (heavy + slow) when nothing is known, so a
///    metadata-less model is treated cautiously rather than optimistically.
pub fn footprint_for(
    name: &str,
    params_b: Option<f64>,
    quant: Option<&str>,
    ctx: u32,
) -> ModelFootprint {
    if let Some(fp) = lookup_footprint(name) {
        // Deref then clone to yield an owned `ModelFootprint` from the static ref.
        return (*fp).clone();
    }
    match params_b {
        Some(p) => estimate_footprint(p, quant.unwrap_or("Q4_K_M"), ctx),
        None => ModelFootprint {
            name: "unknown",
            weights_bytes: 7 * 1024 * 1024 * 1024, // ~7 GB — assume a mid-size model
            kv_bytes_per_ctx_token: 4096,
            default_num_ctx: ctx.max(1),
            prompt_eval_tok_s_apple_m3: 200.0,
            gen_tok_s_apple_m3: 15.0,
            gen_tok_s_generic: 5.0,
        },
    }
}

// ── Resource profiles ─────────────────────────────────────────────────────────

/// How aggressively Indexa should use machine resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResourceProfile {
    /// Keep ≥8 GB free; unify to one model tier; short keep_alive.
    /// Best when running other heavy apps alongside Indexa.
    Conservative,
    /// Keep ≥5 GB free; allow distinct file+dir models; 30 s keep_alive.
    /// The safe default for most machines.
    #[default]
    Balanced,
    /// Keep ≥3 GB free; longer keep_alive to reduce model-load overhead.
    /// Fastest, but not safe for heavy multitasking.
    Performance,
}

impl ResourceProfile {
    /// Minimum headroom in bytes to keep free.
    pub fn headroom_bytes(self) -> u64 {
        match self {
            Self::Conservative => 8 * 1024 * 1024 * 1024,
            Self::Balanced => 5 * 1024 * 1024 * 1024,
            Self::Performance => 3 * 1024 * 1024 * 1024,
        }
    }

    /// keep_alive value to send with every Ollama request (seconds).
    /// A value of 0 means "unload immediately after this request."
    pub fn keep_alive_secs(self) -> i64 {
        match self {
            Self::Conservative => 15,
            Self::Balanced => 30,
            Self::Performance => 120,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }
}

// ── Memory budget & model-fit ─────────────────────────────────────────────────

/// The effective memory budget available for a new model load.
///
/// Uses `min(spec.gpu_wired_limit, total - used) - headroom`.
///
/// We compute available as `total_ram - used_bytes` where `used_bytes` from
/// sysinfo represents active + wired pages (truly in-use, not reclaimable).
/// This correctly excludes the ~10-15 GB of inactive file cache that macOS
/// keeps as reclaimable buffer — that cache is returned to Ollama instantly
/// when needed.  Using `free_bytes` or `available_bytes` from sysinfo is
/// unreliable on macOS (sysinfo 0.30 returns 0 for available_memory on macOS).
pub fn compute_budget(spec: &MachineSpec, sample: &MemSample, headroom: u64) -> i64 {
    // Approximate available = total RAM - actively used (wired + active pages).
    let truly_available = spec
        .total_ram_bytes
        .saturating_sub(sample.used_bytes)
        .min(spec.gpu_wired_limit_bytes);
    truly_available as i64 - headroom as i64
}

/// Check whether all models used by a job fit within the current budget.
///
/// Returns `Ok(())` if every model fits, or `Err(message)` describing the
/// first model that doesn't fit.  When `auto_select` is true the caller
/// should downgrade and retry rather than hard-error.
pub fn check_models_fit(
    models: &[&str],
    spec: &MachineSpec,
    sample: &MemSample,
    headroom: u64,
) -> Result<(), String> {
    let budget = compute_budget(spec, sample, headroom);
    for &model in models {
        if let Some(fp) = lookup_footprint(model) {
            let peak = fp.peak_bytes(fp.default_num_ctx) as i64;
            if peak > budget {
                let budget_gb = budget as f64 / (1024.0 * 1024.0 * 1024.0);
                let peak_gb = peak as f64 / (1024.0 * 1024.0 * 1024.0);
                return Err(format!(
                    "model '{model}' needs {peak_gb:.1} GB but only {budget_gb:.1} GB is budgeted. \
                     Run `indexa doctor` for recommendations."
                ));
            }
        }
        // Unknown model: skip fit check, emit a warning later.
    }
    Ok(())
}

// ── Model-fit report (pre-flight "ask me first") ──────────────────────────────

/// The fall-back model used when a configured summarization model is too big to
/// fit the current memory budget. The recommendation ladder swaps the heavy
/// dir-roll-up model down to this floor; the embedder is never downgraded.
pub const FIT_FLOOR_MODEL: &str = "gemma3:4b";

/// One concrete summarization model set and whether it fits the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFit {
    pub file_model: String,
    pub dir_model: String,
    pub num_ctx: u32,
    /// Combined resident footprint — the sum of the **distinct** file + dir model
    /// peaks (they can be co-resident under `keep_alive`; identical names count
    /// once). Excludes the embedder (~0.35 GB), a small known under-count. See
    /// [`resident_peak`].
    pub peak_bytes: u64,
    pub fits: bool,
}

/// A pre-flight model-fit *report* — it reports, it does NOT decide. The caller
/// chooses: the web path surfaces the choice ("ask me first"); the CLI applies
/// `recommended` non-interactively when `auto_select_model` is on.
#[derive(Debug, Clone)]
pub struct FitReport {
    /// `compute_budget` at sample time (may be negative).
    pub budget_bytes: i64,
    /// What the user's config would load.
    pub configured: ModelFit,
    /// A *lighter* set than `configured`, offered when `configured` doesn't fit.
    /// Its own [`ModelFit::fits`] says whether even this lighter set fits — it is
    /// offered even when it doesn't, because a smaller model always reduces the
    /// overcommit (the runtime watchdog covers the rest). `None` only when already
    /// on the lightest models, or when `configured` fits.
    pub recommended: Option<ModelFit>,
    /// Calm UI/CLI text describing the substitution, when one is offered.
    pub reason: Option<String>,
}

fn model_peak(name: &str, num_ctx: u32) -> u64 {
    // Unknown models have no footprint → treat as 0 (skip the fit check) rather
    // than block, matching `check_models_fit`.
    lookup_footprint(name).map_or(0, |fp| fp.peak_bytes(num_ctx))
}

/// Combined resident footprint of the summarization models: the sum of the
/// **distinct** model peaks among {file_model, dir_model}. Ollama keeps each
/// distinct model warm under `keep_alive`, so during summarize the file and dir
/// models can be **co-resident** (`ollama ps` confirms this) — hence we sum rather
/// than take the max. Identical model names count once. (The embedder, ~0.35 GB,
/// can also be warm during summarize; it is intentionally not summed here — a
/// small, known under-count, since the embedder is never downgraded.)
fn resident_peak(file_model: &str, dir_model: &str, num_ctx: u32) -> u64 {
    let file_peak = model_peak(file_model, num_ctx);
    if file_model == dir_model {
        file_peak
    } else {
        file_peak + model_peak(dir_model, num_ctx)
    }
}

/// Report whether the configured summarization models fit the current budget, and
/// if not, a lighter set that would. Pure (no I/O) so it is unit-testable and so
/// the web estimate/popover and the CLI agree by construction. Reuses
/// `compute_budget` + `lookup_footprint` — no new memory math.
pub fn fit_report(
    file_model: &str,
    dir_model: &str,
    num_ctx: u32,
    spec: &MachineSpec,
    sample: &MemSample,
    headroom: u64,
) -> FitReport {
    let budget = compute_budget(spec, sample, headroom);
    let configured_peak = resident_peak(file_model, dir_model, num_ctx);
    let configured = ModelFit {
        file_model: file_model.to_owned(),
        dir_model: dir_model.to_owned(),
        num_ctx,
        peak_bytes: configured_peak,
        fits: configured_peak as i64 <= budget,
    };
    if configured.fits {
        return FitReport {
            budget_bytes: budget,
            configured,
            recommended: None,
            reason: None,
        };
    }

    // Doesn't fit — drop the dir model to the floor, and the file model too only
    // if it is heavier than the floor (never upgrade either).
    let rec_dir = FIT_FLOOR_MODEL;
    let rec_file = if model_peak(file_model, num_ctx) > model_peak(FIT_FLOOR_MODEL, num_ctx) {
        FIT_FLOOR_MODEL
    } else {
        file_model
    };
    if rec_dir == dir_model && rec_file == file_model {
        // Already on the lightest models and they still don't fit — nothing to offer.
        return FitReport {
            budget_bytes: budget,
            configured,
            recommended: None,
            reason: None,
        };
    }

    // A lighter set exists. Offer it even if it *also* exceeds the budget: a smaller
    // model always shrinks the overcommit (and the runtime watchdog covers the rest).
    // This is a deliberate choice — it trades the configured model's summary quality
    // for less memory pressure when nothing fits, prioritizing the anti-freeze goal.
    // It is gated by `auto_select_model`, which the user can disable to keep their model.
    const GB: f64 = (1024 * 1024 * 1024) as f64;
    let rec_peak = resident_peak(rec_file, rec_dir, num_ctx);
    let rec_fits = rec_peak as i64 <= budget;
    let reason = if rec_fits {
        format!(
            "{dir_model} needs ~{:.1} GB but only {:.1} GB is budgeted — using {rec_dir} (~{:.1} GB) instead",
            configured_peak as f64 / GB,
            budget.max(0) as f64 / GB,
            rec_peak as f64 / GB,
        )
    } else {
        format!(
            "{dir_model} (~{:.1} GB) far exceeds the {:.1} GB budget — using the lightest model {rec_dir} (~{:.1} GB, still tight); the watchdog will pause if needed",
            configured_peak as f64 / GB,
            budget.max(0) as f64 / GB,
            rec_peak as f64 / GB,
        )
    };
    FitReport {
        budget_bytes: budget,
        configured,
        recommended: Some(ModelFit {
            file_model: rec_file.to_owned(),
            dir_model: rec_dir.to_owned(),
            num_ctx,
            peak_bytes: rec_peak,
            fits: rec_fits,
        }),
        reason: Some(reason),
    }
}

// ── Memory pressure / watchdog ────────────────────────────────────────────────

/// Current memory pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// Enough headroom — proceed with the next Ollama call.
    Ok,
    /// Headroom is tightening (free RAM has dipped into the headroom margin) —
    /// unload idle models and pause briefly.
    Throttle,
    /// Critical — truly-free RAM has fallen below half the headroom floor
    /// (genuine exhaustion); pause longer and unload the resident model.
    Critical,
}

/// Assess current memory pressure for the watchdog.
///
/// **Pressure is judged by genuinely-free RAM versus the headroom floor, via
/// [`compute_budget`] — not by swap fraction.** `compute_budget` keys off
/// `used_bytes` (active + wired pages), so it already excludes the reclaimable
/// macOS file cache that makes raw *free-page* checks misfire — the original
/// reason this function avoided a budget check does not apply to `compute_budget`.
///
/// Sticky macOS swap *fraction* is deliberately NOT a trigger: macOS grows its
/// swap file dynamically and never drains stale pages, so the fraction stays high
/// long after RAM frees. Keying on it produced spurious Critical/Throttle —
/// verified live, pressure read `critical` at swap ~88 % while +5 GB was genuinely
/// free with no job running. Swap *growth* (active paging right now) would be a
/// real signal, but it needs cross-sample deltas this pure single-sample function
/// can't see; the budget captures the same danger, since active swapping drives
/// `used_bytes` up and the budget down.
///
/// Ladder (`H` = `headroom`): `budget > 0` → `Ok`; `-H/2 < budget ≤ 0` → `Throttle`
/// (eating into the safety margin); `budget ≤ -H/2` → `Critical` (truly-free RAM has
/// fallen below half the headroom floor — genuine exhaustion).
pub fn assess(sample: &MemSample, spec: &MachineSpec, headroom: u64) -> Pressure {
    let budget = compute_budget(spec, sample, headroom);
    if budget > 0 {
        return Pressure::Ok;
    }
    // Free RAM is at/below the headroom floor. How far below splits Throttle from
    // Critical: once truly-available RAM is under half the floor, treat as Critical.
    if budget <= -((headroom / 2) as i64) {
        Pressure::Critical
    } else {
        Pressure::Throttle
    }
}

/// Maximum total time a memory-pressure pause may last before the job proceeds
/// anyway, regardless of level. Shared by the CLI worker and the web summarize loop
/// so the two pause paths agree (they previously disagreed: 5 min vs 2 min).
pub const MAX_PAUSE_SECS: u64 = 300;

/// What a memory-pressure pause loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Pressure cleared — resume work.
    Resume,
    /// Pressure persisted past [`MAX_PAUSE_SECS`] — proceed anyway (caller logs it).
    Proceed,
    /// Keep waiting: sleep this many seconds, then re-evaluate.
    Sleep(u64),
}

/// Decide the next pause action from the *current* pressure level and how long the
/// loop has already paused. Pure and sleep-free so it is unit-testable, and re-sampling
/// `current` each tick means an escalation (Throttle → Critical) is reflected immediately.
/// Critical waits in 5 s ticks, Throttle in 2 s ticks; both cap at [`MAX_PAUSE_SECS`].
pub fn pause_decision(current: Pressure, elapsed_secs: u64) -> PauseAction {
    if current == Pressure::Ok {
        PauseAction::Resume
    } else if elapsed_secs >= MAX_PAUSE_SECS {
        PauseAction::Proceed
    } else if current == Pressure::Critical {
        PauseAction::Sleep(5)
    } else {
        PauseAction::Sleep(2)
    }
}

/// Decide the next pause action from a fresh memory sample, resuming the moment memory has
/// **actually recovered** (free RAM back above the headroom floor).
///
/// # Why this gates on the budget directly
/// The real recovery signal is *free RAM returning*, which [`compute_budget`] measures
/// (`total - used - headroom`): when unloading the resident model frees its wired pages,
/// `used_bytes` drops and the budget climbs back above 0 — that is the moment to resume.
/// (Historically this mattered because `assess()` keyed off the *sticky* macOS swap fraction
/// and so never cleared after a Critical pause; `assess()` is now budget-aware too (Branch S),
/// so the explicit budget check below and `assess()` agree — the check is the clear,
/// load-bearing recovery signal and stays as defense-in-depth.)
///
/// Resume when `compute_budget(spec, sample, headroom) > 0` (truly-available RAM exceeds
/// headroom). Otherwise it delegates to [`pause_decision`] for the unchanged 5 s (Critical) /
/// 2 s (Throttle) tick cadence and the [`MAX_PAUSE_SECS`] → `Proceed` backstop. Pure and
/// sleep-free so it is unit-testable; both the web loop and the CLI worker call it so the
/// two pause paths agree.
pub fn pause_step(
    spec: &MachineSpec,
    sample: &MemSample,
    headroom: u64,
    elapsed_secs: u64,
) -> PauseAction {
    if compute_budget(spec, sample, headroom) > 0 {
        // RAM recovered — resume even if swap is still sticky-high.
        return PauseAction::Resume;
    }
    // No recovery yet: fall back to swap-based cadence + the time backstop. `assess() == Ok`
    // (entry signal cleared) also resolves to Resume here.
    pause_decision(assess(sample, spec, headroom), elapsed_secs)
}

// ── ETA estimation ────────────────────────────────────────────────────────────

/// Per-model ETA estimates.
#[derive(Debug, Clone)]
pub struct EtaEstimate {
    pub model_name: String,
    /// Estimated seconds per file summary (includes prompt eval + generation).
    pub secs_per_file: f64,
    /// Estimated seconds per embedding chunk.
    pub secs_per_embed_chunk: f64,
    /// Estimated total seconds for `n_files` files + `n_chunks` embed chunks.
    pub total_secs: f64,
    /// Human-readable total time.
    pub display: String,
}

/// Estimate how long a job will take for a given model and workload.
///
/// `avg_file_tokens` is the estimated average token count of each file's
/// content sample (prompt + response).  Defaults to ~600 if unknown.
/// `n_passes` is how many refinement passes will be done per file.
pub fn estimate_eta(
    model_name: &str,
    n_files: usize,
    n_chunks: usize,
    avg_file_tokens: u32,
    n_passes: u32,
    is_apple_m3: bool,
) -> EtaEstimate {
    let avg_file_tokens = if avg_file_tokens == 0 {
        600
    } else {
        avg_file_tokens
    };
    let n_passes = n_passes.max(1);

    let (prompt_tok_s, gen_tok_s, embed_per_min) = if let Some(fp) = lookup_footprint(model_name) {
        let (p, g) = throughput(fp, is_apple_m3);
        // Embed throughput in chunks/min: embedders are fast; nomic ~400/min on M3 Max.
        let e = if is_apple_m3 { 400.0 } else { 120.0 };
        (p, g, e)
    } else {
        // Unknown model — use very conservative defaults.
        (200.0, 15.0, 80.0)
    };

    // Per-file: prompt eval dominates for large files, gen for short ones.
    let prompt_secs = avg_file_tokens as f64 / prompt_tok_s;
    let gen_secs = 100.0 / gen_tok_s; // ~100 output tokens per summary
    let secs_per_file = (prompt_secs + gen_secs) * n_passes as f64;
    let secs_per_embed_chunk = 60.0 / embed_per_min;

    let total_secs = secs_per_file * n_files as f64 + secs_per_embed_chunk * n_chunks as f64;

    let display = format_duration(total_secs as u64);

    EtaEstimate {
        model_name: model_name.to_owned(),
        secs_per_file,
        secs_per_embed_chunk,
        total_secs,
        display,
    }
}

/// M3 vs non-M3 (prompt_eval, generation) throughput selection for a footprint,
/// in tokens/sec. Factored out of [`estimate_eta`] so [`estimate_eta_with`]
/// reuses identical arithmetic — one source of truth, no drift.
fn throughput(fp: &ModelFootprint, is_apple_m3: bool) -> (f64, f64) {
    let prompt_tok_s = if is_apple_m3 {
        fp.prompt_eval_tok_s_apple_m3
    } else {
        fp.prompt_eval_tok_s_apple_m3 * 0.3 // conservative non-M3 fallback
    };
    let gen_tok_s = if is_apple_m3 {
        fp.gen_tok_s_apple_m3
    } else {
        fp.gen_tok_s_generic
    };
    (prompt_tok_s, gen_tok_s)
}

/// ETA for an arbitrary [`ModelFootprint`] — lets catalog / unknown models that
/// have no [`MODEL_FOOTPRINTS`] entry get real estimates via [`footprint_for`].
///
/// `embed_per_min` is a **workload-class** constant (embedding vs generation),
/// not a model property, so the caller supplies it (M3 ≈ 400, non-M3 ≈ 120,
/// unknown ≈ 80). The per-file / per-chunk math mirrors [`estimate_eta`] exactly
/// via the shared [`throughput`] helper.
pub fn estimate_eta_with(
    fp: &ModelFootprint,
    n_files: usize,
    n_chunks: usize,
    avg_file_tokens: u32,
    n_passes: u32,
    is_apple_m3: bool,
    embed_per_min: f64,
) -> EtaEstimate {
    let avg_file_tokens = if avg_file_tokens == 0 {
        600
    } else {
        avg_file_tokens
    };
    let n_passes = n_passes.max(1);

    let (prompt_tok_s, gen_tok_s) = throughput(fp, is_apple_m3);

    let prompt_secs = avg_file_tokens as f64 / prompt_tok_s;
    let gen_secs = 100.0 / gen_tok_s; // ~100 output tokens per summary
    let secs_per_file = (prompt_secs + gen_secs) * n_passes as f64;
    let secs_per_embed_chunk = 60.0 / embed_per_min;

    let total_secs = secs_per_file * n_files as f64 + secs_per_embed_chunk * n_chunks as f64;
    let display = format_duration(total_secs as u64);

    EtaEstimate {
        model_name: fp.name.to_owned(),
        secs_per_file,
        secs_per_embed_chunk,
        total_secs,
        display,
    }
}

/// Public alias for use in other crates (e.g. `apps/indexa`).
pub fn format_duration_pub(secs: u64) -> String {
    format_duration(secs)
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("~{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("~{m}m")
        } else {
            format!("~{m}m {s}s")
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("~{h}h")
        } else {
            format!("~{h}h {m}m")
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_spec(total_gb: u64, apple_silicon: bool) -> MachineSpec {
        let total = total_gb * 1024 * 1024 * 1024;
        let gpu_limit = if apple_silicon {
            (total as f64 * 0.75) as u64
        } else {
            total
        };
        MachineSpec {
            total_ram_bytes: total,
            physical_cores: 10,
            logical_cores: 14,
            is_apple_silicon: apple_silicon,
            gpu_wired_limit_bytes: gpu_limit,
        }
    }

    fn fake_sample(free_gb: u64, swap_used_mb: u64, swap_total_mb: u64) -> MemSample {
        MemSample {
            free_bytes: free_gb * 1024 * 1024 * 1024,
            available_bytes: free_gb * 1024 * 1024 * 1024,
            // Simulate used memory as total - free (so budget = total - used = free)
            // This is used for compute_budget which uses total - used_bytes.
            used_bytes: 0, // callers that care about budget set used_bytes explicitly
            swap_used_bytes: swap_used_mb * 1024 * 1024,
            swap_total_bytes: swap_total_mb * 1024 * 1024,
        }
    }

    fn fake_sample_with_used(
        free_gb: u64,
        used_gb: u64,
        swap_used_mb: u64,
        swap_total_mb: u64,
    ) -> MemSample {
        MemSample {
            free_bytes: free_gb * 1024 * 1024 * 1024,
            available_bytes: free_gb * 1024 * 1024 * 1024,
            used_bytes: used_gb * 1024 * 1024 * 1024,
            swap_used_bytes: swap_used_mb * 1024 * 1024,
            swap_total_bytes: swap_total_mb * 1024 * 1024,
        }
    }

    #[test]
    fn gemma3_4b_fits_on_36gb_balanced() {
        let spec = fake_spec(36, true);
        // 16 GB used → total - used = 20 GB → budget = 20 - 5 = 15 GB → fits
        let sample = fake_sample_with_used(1, 16, 0, 2048);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        assert!(check_models_fit(&["gemma3:4b"], &spec, &sample, headroom).is_ok());
    }

    #[test]
    fn model_doesnt_fit_when_active_memory_too_high() {
        let spec = fake_spec(36, true);
        // 34 GB actively used → total - used = 2 GB → budget = 2 - 5 = -3 GB
        // gemma3:12b needs ~9 GB → doesn't fit
        let sample = fake_sample_with_used(0, 34, 0, 2048);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let result = check_models_fit(&["gemma3:12b"], &spec, &sample, headroom);
        assert!(result.is_err());
    }

    #[test]
    fn fit_report_fits_when_budget_is_roomy() {
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 15, 0, 2048); // budget ≈ 16 GB
        let p4 = lookup_footprint("gemma3:4b").unwrap().peak_bytes(4096) as i64;
        let p12 = lookup_footprint("gemma3:12b").unwrap().peak_bytes(4096) as i64;
        let budget = compute_budget(&spec, &sample, headroom);
        // Roomy = even the co-resident sum of 4b + 12b fits.
        assert!(
            p4 + p12 <= budget,
            "test budget must accommodate 4b + 12b co-resident"
        );
        let r = fit_report("gemma3:4b", "gemma3:12b", 4096, &spec, &sample, headroom);
        assert!(r.configured.fits);
        assert!(r.recommended.is_none());
        assert!(r.reason.is_none());
    }

    #[test]
    fn fit_report_recommends_floor_when_dir_model_too_big() {
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 25, 0, 2048); // budget ≈ 6 GB
        let p4 = lookup_footprint("gemma3:4b").unwrap().peak_bytes(4096) as i64;
        let p12 = lookup_footprint("gemma3:12b").unwrap().peak_bytes(4096) as i64;
        let budget = compute_budget(&spec, &sample, headroom);
        // The 4b floor fits, but the co-resident 4b + 12b sum does not.
        assert!(
            p4 <= budget && budget < p4 + p12,
            "test budget must sit between the floor and the 4b+12b co-resident sum"
        );
        let r = fit_report("gemma3:4b", "gemma3:12b", 4096, &spec, &sample, headroom);
        assert!(!r.configured.fits);
        let rec = r.recommended.expect("should recommend a fitting set");
        assert_eq!(rec.dir_model, "gemma3:4b");
        assert!(rec.fits);
        assert!(r.reason.is_some());
    }

    #[test]
    fn fit_report_offers_lightest_even_when_it_does_not_fit() {
        // Budget ≈ 2 GB: even the floor (4b) exceeds it. But 4b is lighter than the
        // configured 12b, so it's still offered (least-bad) with fits=false — the CLI
        // loads 4b instead of 12b and minimizes the overcommit.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 29, 0, 2048);
        let p4 = lookup_footprint("gemma3:4b").unwrap().peak_bytes(4096) as i64;
        let budget = compute_budget(&spec, &sample, headroom);
        assert!(
            budget < p4,
            "test budget must be below even the floor model"
        );
        let r = fit_report("gemma3:4b", "gemma3:12b", 4096, &spec, &sample, headroom);
        assert!(!r.configured.fits);
        let rec = r
            .recommended
            .expect("offers the lightest set even when it doesn't fit");
        assert_eq!(rec.dir_model, "gemma3:4b");
        assert!(!rec.fits, "the floor itself is over budget here");
        assert!(r.reason.is_some());
    }

    #[test]
    fn fit_report_no_downgrade_when_already_on_floor() {
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        // budget ≈ 2 GB so even the floor doesn't fit, AND the dir model is already
        // the floor → there is nothing lighter to offer.
        let sample = fake_sample_with_used(0, 29, 0, 2048);
        let r = fit_report("gemma3:4b", "gemma3:4b", 4096, &spec, &sample, headroom);
        assert!(!r.configured.fits);
        assert!(r.recommended.is_none());
    }

    #[test]
    fn no_swap_and_plenty_of_free_is_ok() {
        let spec = fake_spec(36, true);
        let sample = fake_sample(18, 0, 2048);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let pressure = assess(&sample, &spec, headroom);
        assert_eq!(pressure, Pressure::Ok);
    }

    #[test]
    fn sticky_swap_with_free_ram_is_ok() {
        // THE BRANCH-S FIX: macOS swap is sticky and stays high long after RAM frees.
        // A high swap fraction with genuinely-free RAM (budget > 0) must NOT read as
        // pressure — this is the exact false positive that fired "critical" at swap
        // ~88 % while RAM was free, with no job running.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 10, 1800, 2048); // swap ~88 %, only 10 GB used
        assert!(compute_budget(&spec, &sample, headroom) > 0);
        assert_eq!(assess(&sample, &spec, headroom), Pressure::Ok);
    }

    #[test]
    fn genuinely_low_free_ram_triggers_critical() {
        // Real exhaustion drives the trigger now, NOT swap: 34 GB of 36 GB actively used
        // → ~2 GB truly free → budget = -3 GB, below -headroom/2 → Critical. Swap is 0 here
        // to prove the signal is the budget, not the swap fraction.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 34, 0, 2048);
        assert!(compute_budget(&spec, &sample, headroom) <= -(headroom as i64 / 2));
        assert_eq!(assess(&sample, &spec, headroom), Pressure::Critical);
    }

    #[test]
    fn mild_tightening_into_headroom_is_throttle() {
        // Free RAM dips into the headroom margin but not critically: 32 GB used → ~4 GB
        // truly free → budget = -1 GB, in (-headroom/2, 0] → Throttle, not Critical.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes(); // 5 GB
        let sample = fake_sample_with_used(0, 32, 0, 2048);
        let budget = compute_budget(&spec, &sample, headroom);
        assert!(budget <= 0 && budget > -(headroom as i64 / 2));
        assert_eq!(assess(&sample, &spec, headroom), Pressure::Throttle);
    }

    #[test]
    fn eta_estimate_is_non_zero() {
        let est = estimate_eta("gemma3:4b", 100, 500, 600, 2, true);
        assert!(est.total_secs > 0.0);
        assert!(!est.display.is_empty());
    }

    #[test]
    fn profile_headroom_ordering() {
        assert!(
            ResourceProfile::Conservative.headroom_bytes()
                > ResourceProfile::Balanced.headroom_bytes()
        );
        assert!(
            ResourceProfile::Balanced.headroom_bytes()
                > ResourceProfile::Performance.headroom_bytes()
        );
    }

    #[test]
    fn pause_decision_resumes_when_pressure_clears() {
        assert_eq!(pause_decision(Pressure::Ok, 0), PauseAction::Resume);
        assert_eq!(
            pause_decision(Pressure::Ok, MAX_PAUSE_SECS),
            PauseAction::Resume
        );
    }

    #[test]
    fn pause_decision_sleep_interval_tracks_current_level() {
        // Re-sampled each tick, so an escalation changes the cadence immediately.
        assert_eq!(pause_decision(Pressure::Throttle, 0), PauseAction::Sleep(2));
        assert_eq!(pause_decision(Pressure::Critical, 0), PauseAction::Sleep(5));
        assert_eq!(
            pause_decision(Pressure::Throttle, 100),
            PauseAction::Sleep(2)
        );
    }

    // ── pause_step: recover-aware resume (the freeze fix) ──────────────────────

    #[test]
    fn pause_step_resumes_on_recovery_despite_sticky_swap() {
        // Sticky swap pinned at 100%, BUT only 10 GB of 36 GB is actively used →
        // compute_budget ≈ 21 GB > 0 (RAM recovered, e.g. after unloading the model).
        // Post Branch-S, assess() is budget-aware so it agrees this is Ok; pause_step
        // resumes regardless of the sticky swap.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 10, 2048, 2048); // swap 100%, used 10 GB
        assert!(compute_budget(&spec, &sample, headroom) > 0);
        assert_eq!(assess(&sample, &spec, headroom), Pressure::Ok); // budget healthy → not pressure
        assert_eq!(
            pause_step(&spec, &sample, headroom, 0),
            PauseAction::Resume,
            "RAM recovered → resume regardless of sticky swap"
        );
    }

    #[test]
    fn entry_gate_skips_pause_on_sticky_swap_after_recovery() {
        // The watchdog gates *entry* into a pause on `pause_step(.., 0) == Resume` (skip).
        // With sticky swap pinned at 100% but only 12 GB used, budget ≈ 19 GB > 0, so both
        // the budget-aware `assess()` and pause_step agree there is no pressure — the gate
        // skips and there is no per-file warn/unload/reload thrash.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 12, 2048, 2048); // swap 100%, used 12 GB → budget > 0
        assert_eq!(assess(&sample, &spec, headroom), Pressure::Ok); // budget healthy despite sticky swap
        assert_eq!(
            pause_step(&spec, &sample, headroom, 0),
            PauseAction::Resume,
            "entry gate must skip the pause when RAM has recovered (no per-file thrash)"
        );
    }

    #[test]
    fn pause_step_keeps_sleeping_until_cap_when_unrecovered() {
        // High swap AND no RAM recovery (34 GB used → budget < 0). Sleep in 5 s ticks
        // (Critical) until elapsed ≥ MAX_PAUSE_SECS, then Proceed.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(0, 34, 2048, 2048); // swap 100%, used 34 GB
        assert!(compute_budget(&spec, &sample, headroom) <= 0);
        assert_eq!(
            pause_step(&spec, &sample, headroom, 0),
            PauseAction::Sleep(5)
        );
        assert_eq!(
            pause_step(&spec, &sample, headroom, 100),
            PauseAction::Sleep(5)
        );
        assert_eq!(
            pause_step(&spec, &sample, headroom, MAX_PAUSE_SECS),
            PauseAction::Proceed,
            "unrecovered pressure proceeds at the time backstop"
        );
    }

    #[test]
    fn pause_step_resumes_immediately_with_no_pressure() {
        // No swap, plenty of free RAM → resume on the first check.
        let spec = fake_spec(36, true);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let sample = fake_sample_with_used(20, 8, 0, 2048); // no swap, 8 GB used
        assert_eq!(assess(&sample, &spec, headroom), Pressure::Ok);
        assert_eq!(pause_step(&spec, &sample, headroom, 0), PauseAction::Resume);
    }

    #[test]
    fn pause_decision_proceeds_after_cap_for_both_levels() {
        assert_eq!(
            pause_decision(Pressure::Throttle, MAX_PAUSE_SECS),
            PauseAction::Proceed
        );
        assert_eq!(
            pause_decision(Pressure::Critical, MAX_PAUSE_SECS + 10),
            PauseAction::Proceed
        );
    }

    // ── Footprint heuristic (B1) ──────────────────────────────────────────────

    #[test]
    fn estimate_footprint_lands_near_table_for_mid_large_models() {
        // The heuristic must land within ~20% of the measured table for the
        // mid/large models (the 4B entry is a deliberate outlier — see below).
        let cases = [
            ("gemma2:9b", 9.0_f64),
            ("gemma3:12b", 12.0),
            ("qwen2.5:14b", 14.0),
            ("mistral-small:22b", 22.0),
        ];
        for (name, params_b) in cases {
            let table = lookup_footprint(name).unwrap();
            let est = estimate_footprint(params_b, "Q4_K_M", 4096);
            let w_err = (est.weights_bytes as f64 - table.weights_bytes as f64).abs()
                / table.weights_bytes as f64;
            let pe_err = (est.prompt_eval_tok_s_apple_m3 - table.prompt_eval_tok_s_apple_m3).abs()
                / table.prompt_eval_tok_s_apple_m3;
            assert!(
                w_err < 0.20,
                "{name}: weights err {w_err:.3} (est {} vs table {})",
                est.weights_bytes,
                table.weights_bytes
            );
            assert!(pe_err < 0.20, "{name}: prompt_eval err {pe_err:.3}");
        }
    }

    #[test]
    fn estimate_footprint_4b_is_conservative_outlier() {
        // The 4B table entry is hand-inflated for safety (~1.0 GB/param vs ~0.65
        // for the rest), so the heuristic reads systematically lower. Documented
        // here rather than hidden; a wide band keeps the test honest.
        let table = lookup_footprint("gemma3:4b").unwrap();
        let est = estimate_footprint(4.0, "Q4_K_M", 4096);
        let ratio = est.weights_bytes as f64 / table.weights_bytes as f64;
        assert!(
            (0.50..=1.10).contains(&ratio),
            "4b weights ratio {ratio:.3}"
        );
    }

    #[test]
    fn footprint_for_prefers_exact_table_entry() {
        // An exact table hit wins verbatim — params/quant/ctx are ignored.
        let fp = footprint_for("gemma3:12b", Some(99.0), Some("F16"), 8192);
        assert_eq!(fp.name, "gemma3:12b");
        assert_eq!(fp.weights_bytes, 9 * 1024 * 1024 * 1024);
        assert_eq!(fp.kv_bytes_per_ctx_token, 4096);
    }

    #[test]
    fn footprint_for_estimates_when_no_table_entry() {
        let fp = footprint_for("llama3:70b", Some(70.0), Some("Q4_K_M"), 8192);
        assert_eq!(fp.name, "estimated");
        let expected = (70.0 * 0.65 * 1024.0 * 1024.0 * 1024.0) as u64;
        assert_eq!(fp.weights_bytes, expected);
    }

    #[test]
    fn footprint_for_unknown_defaults_when_no_params() {
        let fp = footprint_for("mystery-model", None, None, 4096);
        assert_eq!(fp.name, "unknown");
        // Heavy + slow conservative posture.
        assert!(fp.weights_bytes >= 6 * 1024 * 1024 * 1024);
        assert!(fp.gen_tok_s_apple_m3 <= 20.0);
    }

    #[test]
    fn quant_scale_maps_known_quants() {
        let cases = [
            ("Q4_K_M", 0.65),
            ("Q4_0", 0.65),
            ("Q5_K_M", 0.80),
            ("Q6_K", 0.95),
            ("Q8_0", 1.15),
            ("Q3_K_M", 0.47),
            ("Q2_K", 0.35),
            ("F16", 2.0),
            ("BF16", 2.0), // must NOT fall through to a Q-branch (starts with 'B')
            ("garbage", 0.65),
        ];
        for (q, expected) in cases {
            assert!(
                (quant_scale(q) - expected).abs() < 1e-9,
                "quant {q}: got {}, want {expected}",
                quant_scale(q)
            );
        }
    }

    #[test]
    fn estimate_eta_with_matches_estimate_eta_on_known_model() {
        // Option-B factoring regression guard: the footprint-driven path must
        // match estimate_eta exactly on a known model (M3 path, embed_per_min
        // 400.0 = estimate_eta's inline literal).
        let baseline = estimate_eta("gemma3:4b", 100, 500, 600, 2, true);
        let fp = footprint_for("gemma3:4b", None, None, 4096);
        let via = estimate_eta_with(&fp, 100, 500, 600, 2, true, 400.0);
        assert!((baseline.secs_per_file - via.secs_per_file).abs() < 1e-9);
        assert!((baseline.secs_per_embed_chunk - via.secs_per_embed_chunk).abs() < 1e-9);
        assert!((baseline.total_secs - via.total_secs).abs() < 1e-9);
    }

    #[test]
    fn estimate_eta_with_produces_real_eta_for_catalog_model() {
        let fp = estimate_footprint(70.0, "Q4_K_M", 8192);
        let eta = estimate_eta_with(&fp, 200, 600, 600, 2, true, 400.0);
        assert!(eta.total_secs > 0.0);
        assert!(!eta.display.is_empty());
    }
}
