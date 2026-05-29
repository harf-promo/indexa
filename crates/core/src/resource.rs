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
//! # macOS `available_memory` caveat
//! `sysinfo::System::available_memory()` on macOS over-counts file cache as
//! "used" and therefore under-reports true free RAM.  The reliable freeze
//! predictor is *swap growth* + *low free pages*, so `assess()` triggers on
//! those signals rather than "available memory."

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

// ── Memory pressure / watchdog ────────────────────────────────────────────────

/// Current memory pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// Enough headroom — proceed with the next Ollama call.
    Ok,
    /// Headroom is tightening — unload idle models and pause briefly.
    Throttle,
    /// Critical — swap is growing or free pages are very low; pause longer.
    Critical,
}

/// Assess current memory pressure for the watchdog.
///
/// **The only reliable freeze signal on macOS is swap usage.**
///
/// On macOS, `free_memory` is near-permanently low (0.3–2.5 GB) because the
/// OS fills all free RAM with reclaimable file cache — this is by design and
/// does NOT indicate pressure. Budget checks using free pages cause false
/// positives that stall jobs on perfectly healthy machines. Only swap growth
/// means the OS genuinely cannot satisfy allocations without thrashing.
///
/// `spec` and `headroom` are accepted for API compatibility but not used here;
/// use `compute_budget` for model-fit pre-flight checks instead.
pub fn assess(sample: &MemSample, _spec: &MachineSpec, _headroom: u64) -> Pressure {
    // Swap in use is the primary (and on macOS, only reliable) freeze signal.
    let swap_fraction = if sample.swap_total_bytes > 0 {
        sample.swap_used_bytes as f64 / sample.swap_total_bytes as f64
    } else {
        // No swap configured — treat any non-zero swap used as pressure.
        if sample.swap_used_bytes > 0 {
            1.0
        } else {
            0.0
        }
    };

    if swap_fraction > 0.5 {
        return Pressure::Critical;
    }
    if swap_fraction > 0.2 {
        return Pressure::Throttle;
    }

    Pressure::Ok
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
        let p = if is_apple_m3 {
            fp.prompt_eval_tok_s_apple_m3
        } else {
            fp.prompt_eval_tok_s_apple_m3 * 0.3 // conservative non-M3 fallback
        };
        let g = if is_apple_m3 {
            fp.gen_tok_s_apple_m3
        } else {
            fp.gen_tok_s_generic
        };
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
    fn no_swap_and_plenty_of_free_is_ok() {
        let spec = fake_spec(36, true);
        let sample = fake_sample(18, 0, 2048);
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let pressure = assess(&sample, &spec, headroom);
        assert_eq!(pressure, Pressure::Ok);
    }

    #[test]
    fn heavy_swap_triggers_critical() {
        let spec = fake_spec(36, true);
        let sample = fake_sample(2, 1500, 2048); // >50 % swap used
        let headroom = ResourceProfile::Balanced.headroom_bytes();
        let pressure = assess(&sample, &spec, headroom);
        assert_eq!(pressure, Pressure::Critical);
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
}
