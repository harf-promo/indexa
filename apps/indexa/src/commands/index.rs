use anyhow::Result;
use indexa_core::config::{self, Config};
use indexa_core::store::Store;

use super::helpers::{index_db_path, preflight_ollama};
use super::{cmd_deep, cmd_scan, cmd_summarize};

#[cfg(unix)]
fn validate_trigger_index_child_target(
    path: &std::path::Path,
    roots: &[std::path::PathBuf],
    expected_dev: u64,
    expected_ino: u64,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let canonical = path.canonicalize()?;
    if !roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| canonical.starts_with(root))
    {
        anyhow::bail!("trigger_index path is outside every indexed root");
    }
    let before = path.metadata()?;
    if !before.is_dir() && !before.is_file() {
        anyhow::bail!("trigger_index target is not a regular file or directory");
    }
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if before.is_dir() {
            libc::O_DIRECTORY
        } else {
            0
        };
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?;
    let metadata = opened.metadata()?;
    if (!metadata.is_dir() && !metadata.is_file())
        || (metadata.dev(), metadata.ino()) != (expected_dev, expected_ino)
    {
        anyhow::bail!("trigger_index path changed after MCP validation");
    }
    Ok(())
}

fn validate_trigger_index_child_env(paths: &[String]) -> Result<()> {
    let roots = std::env::var_os("INDEXA_TRIGGER_INDEX_ROOTS");
    let dev = std::env::var_os("INDEXA_TRIGGER_INDEX_DEV");
    let ino = std::env::var_os("INDEXA_TRIGGER_INDEX_INO");
    if roots.is_none() && dev.is_none() && ino.is_none() {
        return Ok(());
    }
    let (Some(roots), Some(dev), Some(ino)) = (roots, dev, ino) else {
        anyhow::bail!("incomplete internal trigger_index validation data");
    };
    #[cfg(unix)]
    {
        let roots: Vec<std::path::PathBuf> = serde_json::from_str(&roots.to_string_lossy())?;
        let dev = dev.to_string_lossy().parse::<u64>()?;
        let ino = ino.to_string_lossy().parse::<u64>()?;
        if paths.len() != 1 {
            anyhow::bail!("invalid internal trigger_index invocation");
        }
        validate_trigger_index_child_target(std::path::Path::new(&paths[0]), &roots, dev, ino)?;
    }
    #[cfg(not(unix))]
    anyhow::bail!("safe trigger_index validation is unsupported on this platform");
    Ok(())
}

/// One-shot context build: scan → deep embed → summarize.
///
/// Equivalent to running `indexa scan`, `indexa deep`, and `indexa summarize`
/// in sequence, but in a single command — ideal for first-time setup or full
/// refreshes. Each phase prints its own progress.
#[allow(clippy::too_many_arguments)] // thin CLI fan-out; grouping into a struct would just move fields
pub(crate) async fn cmd_index(
    paths: Vec<String>,
    embed_model: Option<String>,
    mode: Option<String>,
    passes: Option<u32>,
    contextual: bool,
    contextual_prefix: bool,
    yes: bool,
    cfg: &Config,
) -> Result<()> {
    validate_trigger_index_child_env(&paths)?;
    // ── Preflight: confirm Ollama is up and required models are pulled ─────────
    preflight_ollama(cfg).await?;

    // ── Phase 1: scan ─────────────────────────────────────────────────────────
    println!("── Phase 1 / 3 · Scan ──────────────────────────────────────");
    cmd_scan(paths.clone(), false, yes, cfg).await?;

    // ── Phase 2: deep embed + code graph ──────────────────────────────────────
    println!("\n── Phase 2 / 3 · Deep context ──────────────────────────────");
    cmd_deep(
        paths.clone(),
        embed_model,
        false,
        mode.clone(),
        contextual,
        contextual_prefix,
        false, // `index` always embeds; `--no-embed` is a `deep`-only fast path
        cfg,
    )
    .await?;

    // ── Phase 3: hierarchical summaries ───────────────────────────────────────
    println!("\n── Phase 3 / 3 · Summaries ─────────────────────────────────");
    cmd_summarize(paths, mode, passes, cfg).await?;

    // ── Phase 4 (quiet): decision detectors ───────────────────────────────────
    // An inbox question is a bonus, never a gate — a detector failure must not
    // fail an index build that already succeeded.
    let questions = match detector_pass(cfg) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("decision detector pass failed: {e:#}");
            0
        }
    };

    println!("\n✓ Context is ready.");
    println!("  Ask:    indexa ask \"<question>\"");
    println!("  Export: indexa export <path> --format xml > context.xml");
    if questions > 0 {
        println!("  {questions} question(s) for you — indexa review list");
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[test]
fn trigger_index_child_rejects_directory_swapped_to_outside_symlink() {
    use std::os::unix::fs::MetadataExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("indexed");
    let target = root.join("target");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let validated = target.metadata().unwrap();
    let expected = (validated.dev(), validated.ino());

    let moved = root.join("moved-target");
    std::fs::rename(&target, &moved).unwrap();
    std::os::unix::fs::symlink(&outside, &target).unwrap();

    let result = validate_trigger_index_child_target(&target, &[root], expected.0, expected.1);
    assert!(
        result.is_err(),
        "child accepted an outside symlink swapped after validation"
    );
}

/// Run the post-index detector pass; returns how many questions it opened. Also runs the
/// application/structure recognition pass (v0.66) as a sibling — it writes deterministic facts,
/// not inbox questions, so a detection failure is logged and never fails the build.
fn detector_pass(cfg: &Config) -> Result<usize> {
    let mut store = Store::open(&index_db_path()?)?;
    let report = indexa_core::decisions::detectors::run_detectors(&mut store, &cfg.review)?;

    // App/structure recognition: persist what each directory is (Rust crate, Next.js app,
    // macOS .app bundle, …). Optional user catalog lives next to the config file.
    let user_fp = config::default_config_path()
        .parent()
        .map(|d| d.join("fingerprints.json"));
    match indexa_core::fingerprint::load(user_fp.as_deref()) {
        Ok(defs) => {
            if let Err(e) = indexa_core::app_detect::detect_directory_apps(&mut store, &defs) {
                tracing::warn!("application detection failed: {e:#}");
            }
        }
        Err(e) => tracing::warn!("fingerprint library load failed: {e:#}"),
    }
    Ok(report.opened)
}
