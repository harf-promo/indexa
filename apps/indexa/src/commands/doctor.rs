use anyhow::Result;
use indexa_core::resource::{
    detect_machine, estimate_eta, format_duration_pub, lookup_footprint, sample_memory_once,
    ResourceProfile,
};

pub(crate) async fn cmd_doctor(
    profile_str: String,
    files_hint: Option<usize>,
    chunks_hint: Option<usize>,
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
    // "Reclaimable" = total - actively used (wired+active); macOS's inactive file
    // cache is reclaimable instantly so it counts as available for new allocations.
    let reclaimable_gb = (spec.total_ram_bytes.saturating_sub(sample.used_bytes)) as f64
        / (1024.0 * 1024.0 * 1024.0);
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
    println!("  RAM    {total_gb:.0} GB total   {reclaimable_gb:.1} GB reclaimable  ({free_gb:.1} GB truly free)");
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
    println!("  To apply on macOS:");
    println!("    launchctl setenv OLLAMA_MAX_LOADED_MODELS 1");
    println!("    launchctl setenv OLLAMA_NUM_PARALLEL 1");
    println!("    launchctl setenv OLLAMA_KEEP_ALIVE 30s");
    println!("    # then quit and relaunch Ollama.app");
    println!();

    // ── Claude subscription provider (provider = "claude-code") ──
    // All checks here are token-free local probes — no model is invoked.
    let cfg =
        indexa_core::config::load(&indexa_core::config::default_config_path()).unwrap_or_default();
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

    Ok(())
}
