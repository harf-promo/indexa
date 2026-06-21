use anyhow::{Context, Result};
use directories::BaseDirs;
use indexa_core::config::{self, Config, SummaryMode};
use indexa_core::resource;
use std::path::PathBuf;

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

/// Quick Ollama readiness check. Returns `Ok(())` if Ollama is reachable and all
/// required models are pulled. On failure, prints actionable guidance and returns `Err`.
///
/// Skips the check entirely when the embedding and describer providers are both non-Ollama
/// (e.g. `claude-code`), so Claude-subscription users are never blocked.
pub(crate) async fn preflight_ollama(cfg: &Config) -> anyhow::Result<()> {
    // Only gate on Ollama providers. If neither provider is Ollama, skip silently.
    let embed_is_ollama = cfg.embedding.provider == "ollama";
    let describer_is_ollama = cfg.describer.provider == "ollama";
    if !embed_is_ollama && !describer_is_ollama {
        return Ok(());
    }

    let base = indexa_llm::OllamaLlm::resolve_base_url(Some(cfg.embedding.base_url.as_str()));

    // Build the list of models the current config needs from Ollama.
    let mut required: Vec<(&str, &str)> = Vec::new();
    if embed_is_ollama {
        required.push((cfg.embedding.model.as_str(), "embeddings"));
    }
    if describer_is_ollama {
        required.push((cfg.describer.file_model.as_str(), "file summaries"));
        if cfg.describer.dir_model != cfg.describer.file_model {
            required.push((cfg.describer.dir_model.as_str(), "dir roll-ups / Q&A"));
        }
    }

    let installed = match indexa_llm::ollama_list_models(&base).await {
        Ok(list) => list,
        Err(_) => {
            eprintln!("❌ Ollama is not running. Start it with: ollama serve");
            anyhow::bail!("Ollama is not reachable at {base}");
        }
    };

    let mut missing = Vec::new();
    for (model, _role) in &required {
        if !model_installed_check(&installed, model) {
            missing.push(*model);
        }
    }
    if !missing.is_empty() {
        return offer_to_pull(&base, &missing).await;
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
    use super::{finalize_export, resolve_roots, ExportSink};
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
}
