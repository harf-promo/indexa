use anyhow::Result;
use indexa_core::resource::{
    detect_machine, estimate_eta, format_duration_pub, lookup_footprint, sample_memory_once,
    ResourceProfile,
};

pub(crate) async fn cmd_doctor(
    profile_str: String,
    files_hint: Option<usize>,
    chunks_hint: Option<usize>,
    apply_ollama_env: bool,
    latency: bool,
) -> Result<()> {
    let profile = match profile_str.as_str() {
        "conservative" => ResourceProfile::Conservative,
        "performance" => ResourceProfile::Performance,
        _ => ResourceProfile::Balanced,
    };

    let spec = detect_machine();
    let sample = sample_memory_once();

    let total_gb = spec.total_ram_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let free_gb = sample.free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    // "Available" = the OS's own available metric (macOS XNU active+inactive+free) —
    // the same basis as compute_budget. Do NOT use total − used_bytes: sysinfo's
    // used_memory() includes the macOS compressor, which would understate this by
    // 10+ GB and disagree with `memory_pressure`.
    let reclaimable_gb = if sample.available_bytes > 0 {
        sample.available_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    } else {
        (spec.total_ram_bytes.saturating_sub(sample.used_bytes)) as f64 / (1024.0 * 1024.0 * 1024.0)
    };
    let wired_limit_gb = spec.gpu_wired_limit_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let headroom_gb = profile.headroom_bytes() as f64 / (1024.0 * 1024.0 * 1024.0);
    use indexa_core::resource::compute_budget;
    let budget_gb = compute_budget(&spec, &sample, profile.headroom_bytes()) as f64
        / (1024.0 * 1024.0 * 1024.0);

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║              indexa doctor — machine profile             ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    // ── Machine spec ──
    println!("Machine");
    if spec.is_apple_silicon {
        println!("  Chip   Apple Silicon (unified memory — CPU+GPU share one pool)");
    } else {
        println!("  Arch   x86-64 / non-Apple");
    }
    // Show reclaimable (total − wired/active) alongside truly-free pages.
    // macOS keeps inactive file cache in "free-looking" RAM; only swap = real pressure.
    println!("  RAM    {total_gb:.0} GB total   {reclaimable_gb:.1} GB available  ({free_gb:.1} GB truly free)");
    println!(
        "  CPU    {} physical cores, {} logical threads",
        spec.physical_cores, spec.logical_cores
    );
    if spec.is_apple_silicon {
        println!(
            "  GPU    Metal — wired ceiling ≈ {wired_limit_gb:.0} GB ({:.0}% of RAM)",
            wired_limit_gb / total_gb * 100.0
        );
    }
    println!();

    // ── Profile & budget ──
    println!("Resource profile: {}", profile.as_str().to_uppercase());
    println!("  Headroom  {headroom_gb:.0} GB (kept free at all times)");
    println!("  Budget    {budget_gb:.1} GB available for AI models right now");
    println!(
        "  keep_alive  {} s (model stays warm in Ollama between calls)",
        profile.keep_alive_secs()
    );
    println!();

    // ── Ollama env-var check ──
    println!("Ollama server settings");
    let max_loaded = std::env::var("OLLAMA_MAX_LOADED_MODELS").ok();
    let num_parallel = std::env::var("OLLAMA_NUM_PARALLEL").ok();
    let keep_alive_env = std::env::var("OLLAMA_KEEP_ALIVE").ok();

    let check = |name: &str, val: Option<String>, recommended: &str| match val {
        Some(v) => println!("  ✅  {name} = {v}"),
        None => println!("  ⚠️   {name} not set — recommended: {recommended}"),
    };
    check(
        "OLLAMA_MAX_LOADED_MODELS",
        max_loaded,
        "1  (prevents multiple models staying resident)",
    );
    check(
        "OLLAMA_NUM_PARALLEL",
        num_parallel,
        "1  (prevents KV-cache multiplication)",
    );
    check(
        "OLLAMA_KEEP_ALIVE",
        keep_alive_env,
        "30s  (lets models unload between jobs)",
    );
    println!();
    println!("  NOTE: these env vars are read by the Ollama server at startup.");
    // The recommended trio (name, value) — single source for both the print and --apply.
    const OLLAMA_ENV: [(&str, &str); 3] = [
        ("OLLAMA_MAX_LOADED_MODELS", "1"),
        ("OLLAMA_NUM_PARALLEL", "1"),
        ("OLLAMA_KEEP_ALIVE", "30s"),
    ];
    if apply_ollama_env {
        apply_ollama_env_vars(&OLLAMA_ENV);
    } else if cfg!(target_os = "macos") {
        println!("  To apply: indexa doctor --apply-ollama-env   (runs launchctl setenv for you)");
        println!("  Or manually:");
        for (k, v) in OLLAMA_ENV {
            println!("    launchctl setenv {k} {v}");
        }
        println!("    # then quit and relaunch Ollama.app");
    } else {
        println!("  To apply, add to your shell profile (then restart Ollama):");
        for (k, v) in OLLAMA_ENV {
            println!("    export {k}={v}");
        }
    }
    println!();

    // Load config once for the probes below (Ollama liveness + Claude provider).
    let cfg =
        indexa_core::config::load(&indexa_core::config::default_config_path()).unwrap_or_default();

    // Blocking prerequisites for "can I index right now?" — filled by the Ollama probe.
    let mut readiness_issues: Vec<String> = Vec::new();

    // ── Ollama liveness probe ──
    // Actually contacts the server (GET /api/tags) and confirms the models the *current
    // config* will use are pulled. This is the check that catches the #1 first-run failure:
    // "Ollama isn't running" or "the model was never pulled".
    println!("Ollama server (liveness)");
    let ollama_base =
        indexa_llm::OllamaLlm::resolve_base_url(Some(cfg.embedding.base_url.as_str()));
    println!("  Base URL  {ollama_base}");
    // Required models = those the current config routes through Ollama.
    let mut required: Vec<(&str, &str)> = Vec::new();
    if cfg.embedding.provider == "ollama" {
        required.push((cfg.embedding.model.as_str(), "embeddings"));
    }
    if cfg.describer.provider == "ollama" {
        required.push((cfg.describer.file_model.as_str(), "file summaries"));
        if cfg.describer.dir_model != cfg.describer.file_model {
            required.push((cfg.describer.dir_model.as_str(), "dir roll-ups / Q&A"));
        }
    }
    match indexa_llm::ollama_list_models(&ollama_base).await {
        Ok(installed) => {
            println!("  ✅  reachable — {} model(s) installed", installed.len());
            for (model, role) in &required {
                if model_installed(&installed, model) {
                    println!("  ✅  {model}  ({role})");
                } else {
                    println!("  ❌  {model} — not pulled; run: ollama pull {model}  ({role})");
                    readiness_issues.push(format!("missing model: {model} (ollama pull {model})"));
                }
            }
        }
        Err(e) => {
            println!("  ❌  not reachable: {e:#}");
            println!("      Start Ollama and retry (macOS: open -a Ollama; or `ollama serve`).");
            readiness_issues.push(format!("Ollama not reachable at {ollama_base}"));
        }
    }
    println!();

    // ── Multimodal readiness (shared with `indexa multimodal`) ──
    // Reports which opt-in parsers (image caption, PDF OCR, audio transcribe, video frames) are
    // ready (their external tools + a vision model installed) and how to enable each.
    let _ = super::multimodal::multimodal_readiness(&cfg).await;
    println!("  Run `indexa multimodal --enable` to turn on every ready feature.");
    println!();

    // ── Optional model-latency probe (--latency) ──
    // Times a tiny embed + generate so a slow / overloaded / wrong-host Ollama is caught HERE,
    // not ten minutes into an index. It loads the models (unlike the checks above), so it's
    // opt-in. The first call includes model load — that cold-start cost is the realistic
    // first-index latency, which is exactly the signal worth seeing.
    let ollama_in_use = cfg.embedding.provider == "ollama" || cfg.describer.provider == "ollama";
    if ollama_in_use && latency {
        use indexa_llm::Generator as _;
        use std::time::Instant;
        println!("Model latency");
        if cfg.embedding.provider == "ollama" {
            match super::helpers::build_embedder(&cfg, None) {
                Ok(embedder) => {
                    let t = Instant::now();
                    match embedder.embed("ping").await {
                        Ok(_) => {
                            let ms = t.elapsed().as_millis();
                            if ms > 2000 {
                                println!("  ⚠️   embedding ({}): {ms} ms — slow; indexing will crawl (Ollama swapping, or a remote/wrong host?)", cfg.embedding.model);
                            } else {
                                println!("  ✅  embedding ({}): {ms} ms", cfg.embedding.model);
                            }
                        }
                        Err(e) => println!("  ❌  embedding probe failed: {e:#}"),
                    }
                }
                Err(e) => println!("  ⚠️   could not build embedder for probe: {e:#}"),
            }
        }
        if cfg.describer.provider == "ollama" {
            let llm = indexa_llm::OllamaLlm::new(&ollama_base, &cfg.describer.file_model);
            let t = Instant::now();
            match llm.generate("Reply with the single word: ok").await {
                Ok(_) => {
                    let ms = t.elapsed().as_millis();
                    if ms > 15000 {
                        println!("  ⚠️   generation ({}): {ms} ms — slow; summaries and answers will lag", cfg.describer.file_model);
                    } else {
                        println!("  ✅  generation ({}): {ms} ms", cfg.describer.file_model);
                    }
                }
                Err(e) => println!("  ❌  generation probe failed: {e:#}"),
            }
        }
        println!();
    } else if ollama_in_use {
        println!("  \u{2139}\u{fe0f}   run `indexa doctor --latency` to time the models (catches a slow Ollama before a big index)\n");
    }

    // ── Claude subscription provider (provider = "claude-code") ──
    // All checks here are token-free local probes — no model is invoked.
    let claude = indexa_llm::claude_status(&cfg.describer.claude_bin).await;
    println!("Claude subscription provider  (set [describer] provider = \"claude-code\")");
    if claude.cli_present {
        let ver = claude
            .cli_version
            .as_deref()
            .map(|v| format!(" (v{v})"))
            .unwrap_or_default();
        println!("  ✅  claude CLI found{ver}");
        if claude.logged_in {
            let plan = claude
                .subscription_type
                .as_deref()
                .unwrap_or("subscription");
            println!(
                "  ✅  signed in — {plan} plan; summaries/answers can run on your subscription"
            );
        } else {
            println!("  ⚠️   not signed in — run `claude login` to use the subscription provider");
        }
    } else {
        println!("  ⚠️   claude CLI not found on PATH — install Claude Code to use provider=\"claude-code\"");
    }
    if cfg.describer.provider == "claude-code" {
        println!(
            "  ℹ️   ACTIVE — [describer] provider = \"claude-code\", model = \"{}\"",
            cfg.describer.model
        );
    } else {
        println!(
            "  ℹ️   currently provider = \"{}\" (local) — embeddings always stay local either way",
            cfg.describer.provider
        );
    }
    println!();

    // ── Per-model memory table ──
    println!("Model memory estimates  (num_ctx=4096, num_parallel=1)");
    println!(
        "  {:<28}  {:>10}  {:>8}  {:>6}",
        "Model", "Peak RAM", "Fits?", "Role"
    );
    println!(
        "  {}  {}  {}  {}",
        "─".repeat(28),
        "─".repeat(10),
        "─".repeat(8),
        "─".repeat(20)
    );
    let models_of_interest = [
        ("nomic-embed-text", "embeddings"),
        ("gemma3:4b", "file summaries"),
        ("gemma3:12b", "dir roll-ups / Q&A"),
    ];
    for (model, role) in &models_of_interest {
        let peak_display = lookup_footprint(model)
            .map(|fp| fp.peak_display(4096))
            .unwrap_or_else(|| "unknown".to_owned());
        let fits = lookup_footprint(model)
            .map(|fp| {
                if fp.peak_bytes(4096) as f64 / (1024.0 * 1024.0 * 1024.0) <= budget_gb {
                    "✅"
                } else {
                    "❌"
                }
            })
            .unwrap_or("?");
        println!(
            "  {:<28}  {:>10}  {:>8}  {}",
            model, peak_display, fits, role
        );
    }
    println!();

    // ── Why it freezes (explanation) ──
    println!("Why Indexa can freeze the machine");
    println!("  By default Ollama keeps each model warm for 5 minutes after use.");
    println!("  If nomic-embed-text + gemma3:4b + gemma3:12b all stay resident");
    println!("  at the same time, combined peak can reach 16–20+ GB.  On a");
    println!("  {total_gb:.0} GB machine that pushes into swap → thrash → freeze.");
    println!();
    println!("  The fix Indexa now enforces:");
    println!(
        "    • keep_alive={} s (models unload faster)",
        profile.keep_alive_secs()
    );
    println!("    • num_parallel=1 per request (no KV-cache multiplication)");
    println!("    • Explicit unload when switching between models");
    println!("    • Pre-flight fit check before each job");
    println!();

    // ── ETA estimates ──
    let n_files = files_hint.unwrap_or(200);
    let n_chunks = chunks_hint.unwrap_or(n_files * 8);
    println!(
        "ETA estimates  (for ~{n_files} files / ~{n_chunks} embed chunks, {} passes)",
        2
    );
    println!(
        "  {:<28}  {:>12}  {:>12}  {:>14}",
        "Gen model", "Embed only", "Summarize", "Total (deep+summarize)"
    );
    println!(
        "  {}  {}  {}  {}",
        "─".repeat(28),
        "─".repeat(12),
        "─".repeat(12),
        "─".repeat(14)
    );
    for (model, _role) in &models_of_interest[1..] {
        // skip embed model
        let embed_eta = estimate_eta("nomic-embed-text", 0, n_chunks, 0, 1, spec.is_apple_silicon);
        let sum_eta = estimate_eta(model, n_files, 0, 600, 2, spec.is_apple_silicon);
        let total_secs = embed_eta.total_secs + sum_eta.total_secs;
        println!(
            "  {:<28}  {:>12}  {:>12}  {:>14}",
            model,
            embed_eta.display,
            sum_eta.display,
            format_duration_pub(total_secs as u64),
        );
    }
    println!();
    println!("  Pass `--files N --chunks M` to customise for your index size.");
    println!("  Run `indexa status` to see how many files are currently indexed.");
    println!();

    // ── Index integrity ──────────────────────────────────────────────────────
    println!("Index integrity");
    match super::helpers::index_db_path() {
        Err(e) => println!("  ⚠️   could not determine DB path: {e:#}"),
        Ok(db_path) if !db_path.exists() => {
            println!(
                "  ℹ️   No index found at {} — run `indexa scan <path>` first.",
                db_path.display()
            );
        }
        Ok(db_path) => {
            match indexa_core::store::Store::open(&db_path) {
                Err(e) => println!("  ❌  Could not open index: {e:#}"),
                Ok(store) => {
                    // SQLite integrity check
                    let integrity: Result<String, _> =
                        store
                            .db_connection()
                            .query_row("PRAGMA integrity_check", [], |r| r.get(0));
                    match integrity {
                        Ok(ref s) if s == "ok" => println!("  ✅  PRAGMA integrity_check → ok"),
                        Ok(ref s) => println!("  ❌  PRAGMA integrity_check → {s}"),
                        Err(e) => println!("  ⚠️   integrity_check failed: {e:#}"),
                    }
                    // Orphan chunks (no matching entry)
                    let orphans: Result<i64, _> = store.db_connection().query_row(
                        "SELECT COUNT(*) FROM chunks WHERE entry_path NOT IN (SELECT path FROM entries)",
                        [],
                        |r| r.get(0),
                    );
                    match orphans {
                        Ok(0) => println!("  ✅  No orphaned chunks"),
                        Ok(n) => println!("  ⚠️   {n} orphaned chunk(s) (no matching entry) — run `indexa scan` to repair"),
                        Err(e) => println!("  ⚠️   orphan check failed: {e:#}"),
                    }
                    // Queue health
                    let stalled: Result<i64, _> = store.db_connection().query_row(
                        "SELECT COUNT(*) FROM summary_queue \
                         WHERE state = 'in_flight' \
                           AND updated_at < (unixepoch() - 600)",
                        [],
                        |r| r.get(0),
                    );
                    match stalled {
                        Ok(0) => println!("  ✅  No stalled queue items (in_flight > 10 min)"),
                        Ok(n) => println!(
                            "  ⚠️   {n} queue item(s) stuck in_flight for >10 min — \
                             likely a crashed worker. Run `indexa worker` to recover."
                        ),
                        Err(e) => println!("  ⚠️   queue stall check failed: {e:#}"),
                    }
                    // Pending queue count
                    let pending: Result<i64, _> = store.db_connection().query_row(
                        "SELECT COUNT(*) FROM summary_queue WHERE state = 'pending'",
                        [],
                        |r| r.get(0),
                    );
                    if let Ok(n) = pending {
                        if n > 0 {
                            println!("  ℹ️   {n} pending summary job(s) in queue — run `indexa worker` to drain.");
                        }
                    }
                    println!("  ℹ️   DB path: {}", db_path.display());
                }
            }
        }
    }
    println!();

    // ── macOS binary codesign health ─────────────────────────────────────────
    #[cfg(target_os = "macos")]
    {
        println!("Binary code-signing (macOS)");
        match std::env::current_exe() {
            Err(e) => println!("  ⚠️   could not determine current binary path: {e}"),
            Ok(exe) => {
                let out = std::process::Command::new("codesign")
                    .args(["--verify", "--verbose=1", exe.to_str().unwrap_or("?")])
                    .output();
                match out {
                    Ok(o) if o.status.success() => {
                        println!("  ✅  {} — signature valid", exe.display());
                    }
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        println!(
                            "  ❌  {} — signature INVALID: {}",
                            exe.display(),
                            err.trim()
                        );
                        println!("  ↳  Fix: codesign --force --sign - {}", exe.display());
                    }
                    Err(e) => println!("  ⚠️   could not run codesign (install Xcode CLT): {e}"),
                }
            }
        }
        println!();
    }

    // ── Version sync ──────────────────────────────────────────────────────────
    // Catch the trap where the desktop app updated but the user's terminal `indexa`
    // (and the MCP server it spawns) stayed stale behind it — serving old behavior
    // with no signal. Local, offline, fast: compares this binary to the installed
    // app's Info.plist version. Fails open (no app / unreadable → "nothing to sync").
    {
        use indexa_update::{Skew, Surface};
        println!("Version sync");
        let skew = indexa_update::detect_skew(env!("CARGO_PKG_VERSION"));
        match &skew {
            Skew::CliBehind { .. } => {
                if let Some(msg) = skew.advice(Surface::Cli) {
                    println!("  ⚠️   {msg}");
                }
            }
            Skew::CliAhead { cli, app } => {
                println!(
                    "  ℹ️   CLI v{cli} is newer than the installed app v{app} (dev build) — fine."
                );
            }
            Skew::InSync => println!("  ✅  CLI matches the installed Indexa app."),
            Skew::Unknown => println!("  ✅  No installed Indexa app detected — nothing to sync."),
        }
        println!();
    }

    // ── Readiness summary ─────────────────────────────────────────────────────
    // The bottom-line answer to "can I index right now?" — driven by the blocking
    // prerequisites the Ollama probe collected (server up + required models pulled).
    println!("Readiness");
    if readiness_issues.is_empty() {
        println!("  ✅  Ready to index — run `indexa index <path>`.");
    } else {
        println!(
            "  ⚠   {} issue(s) to resolve before indexing:",
            readiness_issues.len()
        );
        for issue in &readiness_issues {
            println!("      • {issue}");
        }
    }
    println!();

    Ok(())
}

/// True if `want` (a configured model name) is among Ollama's installed `models`,
/// matching leniently across the implicit `:latest` tag (Ollama reports a model pulled
/// as `nomic-embed-text` as `nomic-embed-text:latest`).
fn model_installed(installed: &[String], want: &str) -> bool {
    installed.iter().any(|m| {
        m == want
            || m == &format!("{want}:latest")
            || (!want.contains(':') && m.split(':').next() == Some(want))
    })
}

/// Apply the recommended Ollama server env vars for `indexa doctor --apply-ollama-env`.
/// macOS persists them via `launchctl setenv` (read by the Ollama app on next launch);
/// off macOS we can't persist a parent shell's environment from a child process, so we
/// print the `export` lines to add. Never silent: it reports exactly what it did.
fn apply_ollama_env_vars(vars: &[(&str, &str)]) {
    #[cfg(target_os = "macos")]
    {
        println!("  Applying via launchctl setenv:");
        let mut ok = true;
        for (k, v) in vars {
            match std::process::Command::new("launchctl")
                .args(["setenv", k, v])
                .status()
            {
                Ok(s) if s.success() => println!("    ✅  {k} = {v}"),
                Ok(s) => {
                    ok = false;
                    println!("    ⚠️   {k}: launchctl exited {:?}", s.code());
                }
                Err(e) => {
                    ok = false;
                    println!("    ⚠️   {k}: could not run launchctl ({e})");
                }
            }
        }
        if ok {
            println!("  Done. Quit and relaunch Ollama.app so the server picks them up.");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!("  Add these to your shell profile, then restart the Ollama service:");
        for (k, v) in vars {
            println!("    export {k}={v}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::model_installed;

    #[test]
    fn matches_exact_and_implicit_latest_tag() {
        // Ollama reports an untagged pull as `name:latest`; an untagged config name must match it.
        let installed = vec![
            "nomic-embed-text:latest".to_owned(),
            "gemma3:12b".to_owned(),
        ];
        assert!(model_installed(&installed, "nomic-embed-text")); // untagged ↔ :latest
        assert!(model_installed(&installed, "gemma3:12b")); // exact tagged match
        assert!(!model_installed(&installed, "gemma3:4b")); // different tag = not installed
        assert!(!model_installed(&installed, "llama3")); // absent
    }

    #[test]
    fn tagged_want_does_not_match_a_different_tag() {
        // A specific tag must not be satisfied by a same-family different tag.
        let installed = vec!["gemma3:4b".to_owned()];
        assert!(!model_installed(&installed, "gemma3:12b"));
        assert!(model_installed(&installed, "gemma3:4b"));
    }
}
