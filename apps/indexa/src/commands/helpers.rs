use anyhow::{Context, Result};
use directories::BaseDirs;
use indexa_core::config::{self, Config, SummaryMode};
use indexa_core::resource;
use std::path::{Path, PathBuf};

/// Post-processing + destination for a rendered export. Shared by `export` and `pack export`
/// so both get secret redaction, the token-budget guard, and `--clipboard` identically.
pub(crate) struct ExportSink {
    /// Scan + redact suspected secrets before the export leaves the machine (default on).
    pub redact: bool,
    /// Warn (or, with `strict_budget`, fail) when the export exceeds this many estimated tokens.
    pub token_budget: Option<usize>,
    /// Turn an over-budget export into a hard error (e.g. for CI), instead of a warning.
    pub strict_budget: bool,
    /// Copy to the OS clipboard instead of writing a file / stdout.
    pub clipboard: bool,
    /// Write to this file instead of stdout (ignored when `clipboard` is set).
    pub output: Option<String>,
}

/// Apply redaction + the token-budget guard, then deliver the export (clipboard / file / stdout).
pub(crate) fn finalize_export(mut out: String, sink: ExportSink) -> Result<()> {
    // 1. Secret redaction (default on) — never let credentials leave the machine in an export.
    if sink.redact {
        let (clean, n) = indexa_query::redact::redact_secrets(&out);
        if n > 0 {
            eprintln!("⚠ Redacted {n} suspected secret(s) from the export.");
        }
        out = clean;
    }
    // 2. Token-budget guard (estimate ≈4 chars/token).
    if let Some(budget) = sink.token_budget {
        let toks = indexa_query::approx_tokens(&out);
        if toks > budget {
            let msg = format!("export is ~{toks} tokens, over the --token-budget of {budget}");
            if sink.strict_budget {
                anyhow::bail!("{msg}");
            }
            eprintln!("⚠ {msg}");
        }
    }
    // 3. Deliver.
    if sink.clipboard {
        copy_to_clipboard(&out)?;
        eprintln!("Copied {} bytes to the clipboard.", out.len());
        return Ok(());
    }
    if let Some(path) = sink.output {
        // Actionable hint when the parent dir is missing, vs a bare OS error.
        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                anyhow::bail!(
                    "cannot write to '{path}': the directory '{}' does not exist. \
                     Create it first or choose an existing output path.",
                    parent.display()
                );
            }
        }
        std::fs::write(&path, &out).with_context(|| format!("writing export to '{path}'"))?;
        println!("Wrote {} bytes to {path}.", out.len());
    } else {
        print!("{out}");
    }
    Ok(())
}

/// Copy text to the OS clipboard via the platform's native command — no extra dependency (which
/// keeps the Linux CI build free of X11 clipboard libs). Tries `pbcopy` (macOS), `clip` (Windows),
/// or `wl-copy`/`xclip` (Linux); returns an actionable error if none is installed.
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "windows")]
    let candidates: &[(&str, &[&str])] = &[("clip", &[])];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[(&str, &[&str])] =
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];

    for (cmd, args) in candidates {
        let mut child = match Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue, // not installed → try the next
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .context("writing to the clipboard process")?;
        }
        if child
            .wait()
            .context("waiting on the clipboard process")?
            .success()
        {
            return Ok(());
        }
    }
    anyhow::bail!(
        "no clipboard tool found — install one (macOS: pbcopy ships built-in; \
         Linux: wl-copy or xclip) or use --output FILE / pipe stdout instead."
    )
}

/// Return the index DB path if it exists, or `None` after printing the standard
/// "no index found" hint. Call sites collapse to:
///
/// ```ignore
/// let Some(db_path) = require_index_db()? else { return Ok(()); };
/// ```
///
/// `cmd_rm` uses a slightly different hint and so opens the DB directly.
pub(crate) fn require_index_db() -> Result<Option<PathBuf>> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa index <path>` first.");
        return Ok(None);
    }
    Ok(Some(db_path))
}

/// Whether an external binary is on PATH (probes `<bin> --version`). Used by the multimodal
/// readiness checks (tesseract / pdftoppm / ffmpeg / whisper-cli). A missing binary ⇒ `false`,
/// never an error. (Mirrors the `have()` probe in `crates/parsers/tests/multimodal_live.rs`.)
pub(crate) fn have(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
        .unwrap_or(false)
}

/// One model Indexa needs from Ollama: which host to check/pull it against (each provider's
/// own resolved `base_url`), the model name, and a human-readable role label.
///
/// `cfg.embedding.base_url` and `cfg.describer.base_url` are independently settable (the web
/// UI's `POST /api/config/provider` handler only ever writes the describer's URL), so the
/// embedder and describer can legitimately point at two different Ollama hosts. Every entry
/// here carries the base URL for *its own* provider — never a single base shared across both.
pub(crate) struct OllamaRequirement {
    pub base: String,
    pub model: String,
    pub role: &'static str,
}

/// Pure (no I/O) core of the Ollama readiness checks: given the current config, which models
/// does Indexa need, and which Ollama host does each one belong to? Shared by
/// [`preflight_ollama`] and `indexa doctor`'s liveness probe so both check/pull/benchmark each
/// provider's models against that provider's own configured host, instead of a single base
/// resolved only from `cfg.embedding.base_url`.
pub(crate) fn ollama_requirements(cfg: &Config) -> Vec<OllamaRequirement> {
    let mut required = Vec::new();
    if cfg.embedding.provider == "ollama" {
        let base = indexa_llm::OllamaLlm::resolve_base_url(Some(cfg.embedding.base_url.as_str()));
        required.push(OllamaRequirement {
            base,
            model: cfg.embedding.model.clone(),
            role: "embeddings",
        });
    }
    if cfg.describer.provider == "ollama" {
        let base = indexa_llm::OllamaLlm::resolve_base_url(Some(cfg.describer.base_url.as_str()));
        required.push(OllamaRequirement {
            base: base.clone(),
            model: cfg.describer.file_model.clone(),
            role: "file summaries",
        });
        if cfg.describer.dir_model != cfg.describer.file_model {
            required.push(OllamaRequirement {
                base,
                model: cfg.describer.dir_model.clone(),
                role: "dir roll-ups / Q&A",
            });
        }
    }
    required
}

/// Quick Ollama readiness check. Returns `Ok(())` if Ollama is reachable and all
/// required models are pulled. On failure, prints actionable guidance and returns `Err`.
///
/// Skips the check entirely when the embedding and describer providers are both non-Ollama
/// (e.g. `claude-code`), so Claude-subscription users are never blocked.
///
/// Checks each provider's models against *that provider's own* resolved base URL (see
/// [`ollama_requirements`]) — the embedder and describer can be configured to point at two
/// different Ollama hosts — while still doing only one `/api/tags` call per distinct host
/// (most setups run a single shared local Ollama for both providers).
pub(crate) async fn preflight_ollama(cfg: &Config) -> anyhow::Result<()> {
    let required = ollama_requirements(cfg);
    if required.is_empty() {
        return Ok(());
    }

    // Query each distinct Ollama host once, in first-seen order.
    let mut bases: Vec<&str> = Vec::new();
    for req in &required {
        if !bases.contains(&req.base.as_str()) {
            bases.push(&req.base);
        }
    }
    let mut installed_by_base: std::collections::HashMap<&str, Vec<String>> =
        std::collections::HashMap::new();
    for base in bases {
        match indexa_llm::ollama_list_models(base).await {
            Ok(list) => {
                installed_by_base.insert(base, list);
            }
            Err(_) => {
                eprintln!("❌ Ollama is not reachable at {base}. Start it with: ollama serve");
                anyhow::bail!("Ollama is not reachable at {base}");
            }
        }
    }

    // Missing models, grouped by the host they must be pulled from (preserves entry order).
    let mut missing_by_base: Vec<(&str, Vec<&str>)> = Vec::new();
    for req in &required {
        let installed = installed_by_base
            .get(req.base.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if !model_installed_check(installed, &req.model) {
            match missing_by_base
                .iter_mut()
                .find(|(base, _)| *base == req.base.as_str())
            {
                Some((_, models)) => models.push(req.model.as_str()),
                None => missing_by_base.push((req.base.as_str(), vec![req.model.as_str()])),
            }
        }
    }

    for (base, missing) in &missing_by_base {
        offer_to_pull(base, missing).await?;
    }
    Ok(())
}

/// Offer to pull the missing Ollama models (interactive), rendering a live per-model progress
/// bar. In a non-interactive shell (piped / CI) it keeps the actionable manual instruction and
/// fails fast, so a script never blocks on a prompt. The download has no overall timeout.
async fn offer_to_pull(base: &str, missing: &[&str]) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        for m in missing {
            eprintln!("❌ Model '{m}' not pulled. Run: ollama pull {m}");
        }
        anyhow::bail!("{} required model(s) not pulled", missing.len());
    }

    println!(
        "\nIndexa needs {} local model(s) that aren't pulled yet:",
        missing.len()
    );
    for m in missing {
        println!("  • {m}");
    }
    print!("Download them now via Ollama? [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_lowercase();
    if ans == "n" || ans == "no" {
        for m in missing {
            eprintln!("Skipped — to pull manually later: ollama pull {m}");
        }
        anyhow::bail!("required models not pulled");
    }

    let show = std::io::stderr().is_terminal();
    for m in missing {
        println!("Pulling {m} …");
        indexa_llm::ollama_pull(base, m, |status, completed, total| {
            if !show {
                return;
            }
            let pct = match (completed, total) {
                (Some(c), Some(t)) if t > 0 => format!(" {}%", c * 100 / t),
                _ => String::new(),
            };
            eprint!("\r\x1b[K  {m}: {status}{pct}");
            let _ = std::io::stderr().flush();
        })
        .await
        .map_err(|e| anyhow::anyhow!("pulling {m}: {e:#}"))?;
        if show {
            eprintln!("\r\x1b[K  {m}: done ✓");
        }
    }
    println!("All required models pulled. ✓");
    Ok(())
}

/// Lenient model-name match: `nomic-embed-text` ↔ `nomic-embed-text:latest`.
fn model_installed_check(installed: &[String], want: &str) -> bool {
    installed.iter().any(|m| {
        m == want
            || m == &format!("{want}:latest")
            || (!want.contains(':') && m.split(':').next() == Some(want))
    })
}

/// Build an embedder from config, optionally overriding the model name.
/// Respects `cfg.resource.effective_keep_alive_secs()` for Ollama.
pub(crate) fn build_embedder(
    cfg: &Config,
    model_override: Option<&str>,
) -> Result<Box<dyn indexa_embed::Embedder + Send + Sync>> {
    let model = model_override.unwrap_or(&cfg.embedding.model);
    let keep_alive = cfg.resource.effective_keep_alive_secs();
    indexa_embed::from_config_with_keep_alive(
        &cfg.embedding.provider,
        model,
        cfg.embedding.dim,
        &cfg.embedding.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.google.as_deref(),
        Some(keep_alive),
        cfg.describer.num_ctx,
    )
}

/// Build an LLM generator from config, optionally overriding the model name.
/// Respects `cfg.resource.effective_keep_alive_secs()` for Ollama.
pub(crate) fn build_llm(
    cfg: &Config,
    model_override: Option<&str>,
) -> Result<Box<dyn indexa_llm::Generator + Send + Sync>> {
    let model = model_override.unwrap_or(&cfg.describer.model);
    let keep_alive = cfg.resource.effective_keep_alive_secs();
    indexa_llm::from_config_with_keep_alive(
        &cfg.describer.provider,
        model,
        &cfg.describer.base_url,
        cfg.api_keys.openai.as_deref(),
        cfg.api_keys.anthropic.as_deref(),
        cfg.api_keys.cerebras.as_deref(),
        Some(keep_alive),
        cfg.describer.num_ctx,
    )
}

/// Pick the summarization `(file_model, dir_model)`, downgrading the heavy dir
/// roll-up model to one that fits the live memory budget when `[resource]
/// auto_select_model` is on (the default — the non-interactive CLI behavior).
///
/// This is the CLI side of "ask me first": the CLI can't prompt, so it applies
/// the fitting model and prints a calm notice. The web path surfaces the choice
/// interactively (a separate change). Without this, `summarize`/`worker` load
/// `gemma3:12b` (~9 GB) unconditionally, which on a tight machine thrashes/freezes.
pub(crate) fn select_summary_models(cfg: &Config) -> (String, String) {
    let file_model = cfg.describer.file_model.clone();
    let dir_model = cfg.describer.dir_model.clone();
    if !cfg.resource.auto_select_model {
        return (file_model, dir_model);
    }

    let spec = resource::detect_machine();
    let sample = resource::sample_memory_once();
    let headroom = cfg.resource.effective_headroom_bytes();
    let report = resource::fit_report(
        &file_model,
        &dir_model,
        cfg.describer.num_ctx,
        &spec,
        &sample,
        headroom,
    );

    if let (Some(rec), Some(reason)) = (report.recommended.as_ref(), report.reason.as_ref()) {
        println!("⚠ Memory: {reason}.");
        println!("  (Set [resource] auto_select_model = false in config.toml to keep your configured models.)");
        return (rec.file_model.clone(), rec.dir_model.clone());
    }
    if !report.configured.fits {
        // recommended is None here → already on the smallest model and it still
        // doesn't fit; warn and let the runtime watchdog handle the pressure.
        let to_gb = |b: f64| b / (1024.0 * 1024.0 * 1024.0);
        println!(
            "⚠ Memory: {dir_model} (~{:.1} GB) exceeds the {:.1} GB budget and it's already the \
smallest model. Free some RAM or lower the resource profile; the memory watchdog will pause under pressure.",
            to_gb(report.configured.peak_bytes as f64),
            to_gb(report.budget_bytes as f64),
        );
    }
    (file_model, dir_model)
}

/// Canonicalize a root so scan/deep/watch/rm all agree on its path form. `notify`
/// (watch) reports canonical event paths, so a symlinked root — e.g. macOS /tmp →
/// /private/tmp — would otherwise mismatch the non-canonical path scan stored,
/// producing duplicate queue rows and missed re-summarization. Falls back to the
/// input when it can't be resolved (e.g. doesn't exist yet). Applied to *every*
/// branch so a bare-home root and an explicit path land in the same form.
fn canonical_root(p: PathBuf) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => strip_verbatim_prefix(c),
        Err(_) => p,
    }
}

/// On Windows, `canonicalize` returns a `\\?\` verbatim path; strip it so stored
/// roots stay comparable to `notify`'s non-verbatim event paths and to user-facing
/// display. No-op on Unix.
#[cfg(windows)]
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}
#[cfg(not(windows))]
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    p
}

pub(crate) fn resolve_roots(paths: Vec<String>, all: bool) -> Result<Vec<PathBuf>> {
    if all {
        #[cfg(windows)]
        let root = PathBuf::from("C:\\");
        #[cfg(not(windows))]
        let root = PathBuf::from("/");
        return Ok(vec![canonical_root(root)]);
    }

    if paths.is_empty() {
        let base =
            BaseDirs::new().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        return Ok(vec![canonical_root(base.home_dir().to_path_buf())]);
    }

    Ok(paths
        .into_iter()
        .map(|p| canonical_root(PathBuf::from(shellexpand::tilde(&p).into_owned())))
        .collect())
}

/// Resolve the target roots for `scan`/`deep`/`summarize`/`index`. With explicit `paths` (or `--all`)
/// this is exactly [`resolve_roots`]. With NO paths and not `--all`, it defaults to the **already-indexed
/// roots** (`store.root_paths()`) instead of `$HOME` — so a bare `indexa deep`/`index`/`summarize`
/// re-processes what you've already indexed rather than silently deep-scanning your entire home
/// directory (which the old `resolve_roots` empty-path fallback did). Errors with a hint when nothing
/// is indexed yet.
pub(crate) fn resolve_target_roots(paths: Vec<String>, all: bool) -> Result<Vec<PathBuf>> {
    resolve_target_roots_in(paths, all, index_db_path()?)
}

/// Testable core of [`resolve_target_roots`] with the index DB path injected.
fn resolve_target_roots_in(
    paths: Vec<String>,
    all: bool,
    db_path: PathBuf,
) -> Result<Vec<PathBuf>> {
    if all || !paths.is_empty() {
        return resolve_roots(paths, all);
    }
    let roots = if db_path.exists() {
        indexa_core::store::Store::open(&db_path)?.root_paths()?
    } else {
        Vec::new()
    };
    if roots.is_empty() {
        anyhow::bail!(
            "no indexed roots yet — pass a path (e.g. `indexa index ~/project`) to index something first"
        );
    }
    Ok(roots
        .into_iter()
        .map(|r| canonical_root(PathBuf::from(r)))
        .collect())
}

pub(crate) fn index_db_path() -> Result<PathBuf> {
    let data_dir = config::default_data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    migrate_legacy_data_dir(&data_dir);
    Ok(data_dir.join("index.db"))
}

/// One-time migration: if the old `indexa/` data dir exists but the new canonical
/// `dev.indexa.Indexa/` dir does not, rename it so existing indexes aren't lost.
pub(crate) fn migrate_legacy_data_dir(new_dir: &std::path::Path) {
    if new_dir.exists() {
        return;
    }
    // The old path was `<data_local>/indexa/` (bare name, no qualifier).
    // Derive it by stripping the last component of `new_dir` and appending "indexa".
    if let Some(parent) = new_dir.parent() {
        let old_dir = parent.join("indexa");
        if old_dir.exists() {
            if let Err(e) = std::fs::rename(&old_dir, new_dir) {
                tracing::warn!(
                    "could not migrate data dir {} → {}: {e}",
                    old_dir.display(),
                    new_dir.display()
                );
            } else {
                tracing::info!(
                    "migrated data dir {} → {}",
                    old_dir.display(),
                    new_dir.display()
                );
            }
        }
    }
}

/// Parse the `--mode` flag into a `SummaryMode`, rejecting unknown values with a
/// clear error instead of silently treating a typo (e.g. `compres`) as `augment`.
/// Build a parser registry whose word-window parsers honor `[chunking]` `size`/`overlap`
/// (default 800/100). Every content-parse pipeline (`deep`, `watch`) uses this instead of the
/// free `registry::parse_guarded`, which would rebuild a default-800/100 registry per call.
pub(crate) fn chunk_registry(cfg: &Config) -> indexa_parsers::registry::Registry {
    let mut registry =
        indexa_parsers::registry::Registry::with_chunk(indexa_parsers::types::ChunkParams {
            size: cfg.chunking.size,
            overlap: cfg.chunking.overlap,
            encoding: indexa_parsers::types::TextEncoding::from_config_str(&cfg.parsers.encoding),
        });
    registry.register_preprocessors(&preprocessor_specs(cfg));
    if cfg.parsers.compressed {
        registry.enable_compressed();
    }
    registry
}

/// Convert `[[parsers.preprocessor]]` config entries into parsers-crate-local specs (4.4).
/// `indexa-parsers` has no dependency on `indexa-core`, so this conversion lives at each of
/// the CLI/web call sites — the same pattern `[chunking]` -> `ChunkParams` already uses.
pub(crate) fn preprocessor_specs(
    cfg: &Config,
) -> Vec<indexa_parsers::preprocess::PreprocessorSpec> {
    cfg.parsers
        .preprocessor
        .iter()
        .map(|p| indexa_parsers::preprocess::PreprocessorSpec {
            glob: p.glob.clone(),
            command: p.command.clone(),
            timeout: std::time::Duration::from_secs(p.timeout_s),
            max_output_bytes: p.max_output_mb.saturating_mul(1024 * 1024),
        })
        .collect()
}

pub(crate) fn parse_summary_mode(mode: &str) -> Result<SummaryMode> {
    match mode {
        "augment" => Ok(SummaryMode::Augment),
        "compress" => Ok(SummaryMode::Compress),
        "summaries-only" => Ok(SummaryMode::SummariesOnly),
        other => anyhow::bail!(
            "unknown --mode '{other}'. Valid values: augment, compress, summaries-only"
        ),
    }
}

/// Resolve the effective summary mode for a command invocation: an explicit `--mode` flag
/// always overrides, and an omitted flag defers to `cfg_mode` (the config-loaded
/// `[describer] mode`). Mirrors `build_embedder`'s `model_override.unwrap_or(&cfg.field)`
/// "flag overrides config, omitted flag defers to config" pattern — `--mode` used to be a
/// plain `String` with a hardcoded CLI default, which meant clap always supplied SOME value
/// and the config's `mode` could never win even when the user never typed `--mode`.
pub(crate) fn resolve_summary_mode(
    cli_mode: Option<&str>,
    cfg_mode: SummaryMode,
) -> Result<SummaryMode> {
    match cli_mode {
        Some(m) => parse_summary_mode(m),
        None => Ok(cfg_mode),
    }
}

#[cfg(test)]
mod summary_mode_tests {
    use super::{resolve_summary_mode, SummaryMode};

    #[test]
    fn omitted_flag_defers_to_config_mode() {
        let resolved = resolve_summary_mode(None, SummaryMode::SummariesOnly).unwrap();
        assert_eq!(
            resolved,
            SummaryMode::SummariesOnly,
            "an omitted --mode must preserve whatever [describer] mode config loaded, \
             not silently fall back to augment"
        );
    }

    #[test]
    fn explicit_flag_overrides_config_mode() {
        // Config says summaries-only, but the user explicitly typed --mode augment on
        // this one invocation — the explicit flag must win.
        let resolved = resolve_summary_mode(Some("augment"), SummaryMode::SummariesOnly).unwrap();
        assert_eq!(resolved, SummaryMode::Augment);
    }

    #[test]
    fn explicit_flag_still_validates() {
        let err = resolve_summary_mode(Some("bogus"), SummaryMode::Augment).unwrap_err();
        assert!(err.to_string().contains("unknown --mode"));
    }
}

pub(crate) fn format_size(bytes: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// True when `path` is an unreasonably large scan root: the filesystem root or
/// the user's home directory. Scanning either indexes hundreds of thousands of
/// files and can consume gigabytes of disk/WAL. Guarded by `check_huge_root_guard`.
fn is_huge_root(path: &Path) -> bool {
    // Filesystem root: has no parent (/ on Unix, C:\ on Windows).
    let is_fs_root = path.parent().is_none();
    // User home directory.
    let is_home = BaseDirs::new()
        .map(|b| path == b.home_dir())
        .unwrap_or(false);
    is_fs_root || is_home
}

/// Guard against accidentally indexing the whole home directory or filesystem.
///
/// In an interactive terminal: prompts for confirmation. Non-interactively (e.g. in
/// a CI script or piped command): bails with an error to avoid silently filling disk.
/// Pass `--yes` to skip this check.
pub(crate) fn check_huge_root_guard(root: &Path) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    use std::io::Write as _;

    if !is_huge_root(root) {
        return Ok(());
    }
    if std::io::stdin().is_terminal() {
        print!(
            "About to index a very large tree: {}\n\
             This can index hundreds of thousands of files and consume significant disk. \
             Continue? [y/N] ",
            root.display()
        );
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            anyhow::bail!("Aborted. Pass an explicit path to scan, or use --yes to confirm.");
        }
    } else {
        anyhow::bail!(
            "Refusing to index {} in a non-interactive session. \
             Pass an explicit path, or re-run with --yes to confirm.",
            root.display()
        );
    }
    Ok(())
}

/// Current Unix time in whole seconds (fails open to 0 before the epoch / on a clock error).
/// Single source for the timestamps several commands stamp into snapshots, packs, and reports
/// (was duplicated as `now_str`/`chrono_now`/`now_unix`/`now_secs`). Use `.to_string()` where a
/// string is needed.
pub(crate) fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Expand a leading `~` in a user-supplied path. Shared by the commands that take path args.
pub(crate) fn expand(p: &str) -> String {
    shellexpand::tilde(p).into_owned()
}

/// Format a Unix timestamp (seconds since epoch) as a human-readable UTC datetime
/// like `2026-05-29 14:32 UTC`. Uses Howard Hinnant's civil-date algorithm so we
/// avoid pulling in `chrono` just for this one display string.
pub(crate) fn format_unix_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "unknown".to_owned();
    }
    let secs = ts;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute) = (rem / 3_600, (rem % 3_600) / 60);

    // Civil-from-days (Hinnant): days since 1970-01-01 → (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::{finalize_export, ollama_requirements, resolve_roots, ExportSink};
    use indexa_core::config::Config;
    use std::path::PathBuf;

    /// Build a sink that writes to `output` with all extras off; tests flip individual fields.
    fn sink_to(output: Option<String>) -> ExportSink {
        ExportSink {
            redact: false,
            token_budget: None,
            strict_budget: false,
            clipboard: false,
            output,
        }
    }

    #[test]
    fn finalize_export_strict_budget_bails_when_over() {
        // ~tokens = chars/4 (approx_tokens); 400 chars ⇒ ~100 tokens, well over a budget of 1.
        let content = "x".repeat(400);
        let mut sink = sink_to(None);
        sink.token_budget = Some(1);
        sink.strict_budget = true;
        let err = finalize_export(content, sink).unwrap_err();
        assert!(
            err.to_string().contains("over the --token-budget"),
            "strict over-budget must be a hard error, got: {err}"
        );
    }

    #[test]
    fn finalize_export_over_budget_without_strict_succeeds() {
        // Same over-budget content, but without --strict-budget it only warns (to stderr) and
        // still delivers — here to stdout.
        let content = "x".repeat(400);
        let mut sink = sink_to(None);
        sink.token_budget = Some(1);
        // strict_budget stays false
        assert!(finalize_export(content, sink).is_ok());
    }

    #[test]
    fn finalize_export_missing_parent_dir_bails() {
        // Writing under a parent dir that doesn't exist must be a clear error, not a silent OS failure.
        let pid = std::process::id();
        let missing =
            std::env::temp_dir().join(format!("indexa_no_such_dir_{pid}/export_{pid}.xml"));
        let sink = sink_to(Some(missing.to_string_lossy().into_owned()));
        let err = finalize_export("body".to_owned(), sink).unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "missing-parent write must bail with a directory hint, got: {err}"
        );
    }

    #[test]
    fn finalize_export_within_budget_valid_path_writes_file() {
        // The happy path: in-budget content to an existing dir actually lands on disk.
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("indexa_export_ok_{pid}.xml"));
        let _ = std::fs::remove_file(&path);
        let mut sink = sink_to(Some(path.to_string_lossy().into_owned()));
        sink.token_budget = Some(10_000); // generous — content is tiny
        finalize_export("<export>ok</export>".to_owned(), sink).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "<export>ok</export>");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_roots_canonicalizes_existing_paths() {
        // An existing dir resolves to its canonical form, so scan/deep/watch agree even on
        // symlinked roots (e.g. macOS /tmp → /private/tmp).
        let dir = std::env::temp_dir();
        let got = resolve_roots(vec![dir.to_string_lossy().into_owned()], false).unwrap();
        // On Windows `canonicalize` returns a `\\?\` verbatim path; resolve_roots strips it,
        // so the expected value must strip it too.
        #[allow(unused_mut)]
        let mut expected = dir.canonicalize().unwrap();
        #[cfg(windows)]
        {
            let s = expected.to_string_lossy().into_owned();
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                expected = PathBuf::from(format!(r"\\{rest}"));
            } else if let Some(rest) = s.strip_prefix(r"\\?\") {
                expected = PathBuf::from(rest);
            }
        }
        assert_eq!(got, vec![expected]);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_roots_resolves_a_symlinked_dir() {
        // The real intent: a symlinked root resolves to its canonical target, so a
        // `scan`/`watch` on the symlink agree with notify's canonical event paths.
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().canonicalize().unwrap();
        let pid = std::process::id();
        let target = base.join(format!("indexa_rr_target_{pid}"));
        let link = base.join(format!("indexa_rr_link_{pid}"));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        let got = resolve_roots(vec![link.to_string_lossy().into_owned()], false).unwrap();
        assert_eq!(got, vec![target.clone()]);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn resolve_roots_falls_back_when_path_missing() {
        // A path that can't be canonicalized (doesn't exist yet) falls back to the expanded form.
        let missing = PathBuf::from("/no/such/indexa/path/zzz123");
        let got = resolve_roots(vec![missing.to_string_lossy().into_owned()], false).unwrap();
        assert_eq!(got, vec![missing]);
    }

    #[test]
    fn resolve_target_roots_empty_with_no_index_errors_not_home() {
        // The bug: bare `indexa deep`/`index`/`summarize` (no paths) resolved to $HOME and deep-scanned
        // the whole home dir. Now, with nothing indexed, it errors with a hint instead of falling back.
        let db = std::env::temp_dir().join(format!("indexa_rtr_none_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db); // ensure it doesn't exist
        let err = super::resolve_target_roots_in(vec![], false, db)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no indexed roots"), "got: {err}");
    }

    #[test]
    fn resolve_target_roots_delegates_for_explicit_paths() {
        // Explicit paths behave exactly like resolve_roots regardless of the db path.
        let base = std::env::temp_dir().canonicalize().unwrap();
        let target = base.join(format!("indexa_rtr_target_{}", std::process::id()));
        std::fs::create_dir_all(&target).unwrap();
        let arg = target.to_string_lossy().into_owned();
        let got =
            super::resolve_target_roots_in(vec![arg.clone()], false, base.join("nonexistent.db"))
                .unwrap();
        assert_eq!(got, super::resolve_roots(vec![arg], false).unwrap());
        let _ = std::fs::remove_dir_all(&target);
    }

    /// M9: with the embedder and describer both on Ollama but pointed at DIFFERENT hosts
    /// (reachable via the web UI's `/api/config/provider`, which only ever writes the
    /// describer's URL), each requirement must carry its OWN provider's base — never a single
    /// base resolved only from `cfg.embedding.base_url`.
    #[test]
    fn ollama_requirements_resolves_each_provider_against_its_own_base() {
        let mut cfg = Config::default();
        cfg.embedding.provider = "ollama".into();
        cfg.embedding.base_url = "http://embed-host:11434".into();
        cfg.embedding.model = "nomic-embed-text".into();
        cfg.describer.provider = "ollama".into();
        cfg.describer.base_url = "http://describer-host:11434".into();
        cfg.describer.file_model = "gemma3:4b".into();
        cfg.describer.dir_model = "gemma3:12b".into();

        let got = ollama_requirements(&cfg);
        assert_eq!(got.len(), 3, "embed model + file model + dir model");

        let embed = got.iter().find(|r| r.role == "embeddings").unwrap();
        assert_eq!(embed.base, "http://embed-host:11434");
        assert_eq!(embed.model, "nomic-embed-text");

        let file = got.iter().find(|r| r.role == "file summaries").unwrap();
        assert_eq!(
            file.base, "http://describer-host:11434",
            "the describer's model must check against the DESCRIBER's host, not the embedder's"
        );
        assert_eq!(file.model, "gemma3:4b");

        let dir = got.iter().find(|r| r.role == "dir roll-ups / Q&A").unwrap();
        assert_eq!(dir.base, "http://describer-host:11434");
        assert_eq!(dir.model, "gemma3:12b");
    }

    /// When `describer.dir_model == describer.file_model` (a common config), only one
    /// describer entry is emitted — no duplicate check for the same model.
    #[test]
    fn ollama_requirements_dedupes_identical_file_and_dir_model() {
        let mut cfg = Config::default();
        cfg.embedding.provider = "openai".into(); // embedder not on Ollama at all
        cfg.describer.provider = "ollama".into();
        cfg.describer.file_model = "gemma3:4b".into();
        cfg.describer.dir_model = "gemma3:4b".into();

        let got = ollama_requirements(&cfg);
        assert_eq!(
            got.len(),
            1,
            "identical file/dir model must not be listed twice, and a non-Ollama embedder \
             must contribute nothing"
        );
        assert_eq!(got[0].model, "gemma3:4b");
        assert_eq!(got[0].role, "file summaries");
    }

    /// A provider that isn't Ollama at all (e.g. `claude-code`) contributes no requirement —
    /// no spurious Ollama dependency is introduced for a non-Ollama provider.
    #[test]
    fn ollama_requirements_empty_when_neither_provider_is_ollama() {
        let mut cfg = Config::default();
        cfg.embedding.provider = "openai".into();
        cfg.describer.provider = "claude-code".into();
        assert!(ollama_requirements(&cfg).is_empty());
    }

    /// The common case: both providers ARE Ollama and resolve to the SAME base (most users
    /// run one local Ollama for everything). All three requirements should share that base —
    /// exercised alongside `preflight_ollama`'s de-duplication (see module docs), which relies
    /// on this to avoid a redundant duplicate `/api/tags` call per distinct host.
    #[test]
    fn ollama_requirements_same_host_for_both_providers_when_unset() {
        let cfg = Config::default(); // both default to http://localhost:11434, both "ollama"
        let got = ollama_requirements(&cfg);
        assert_eq!(got.len(), 3);
        let bases: std::collections::HashSet<&str> = got.iter().map(|r| r.base.as_str()).collect();
        assert_eq!(
            bases.len(),
            1,
            "default config: embed and describer share one Ollama host"
        );
    }
}
