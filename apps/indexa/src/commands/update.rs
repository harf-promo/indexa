use anyhow::Result;

pub(crate) async fn cmd_update(check_only: bool, yes: bool, pin: Option<String>) -> Result<()> {
    // ── Check phase ───────────────────────────────────────────────────────────
    let info = indexa_update::check().await?;

    println!("  installed  v{}", info.current);
    println!("  latest     v{}", info.latest);

    if !info.update_available && pin.is_none() {
        println!("Already up to date.");
        if check_only {
            // exit 0 = current
            std::process::exit(0);
        }
        return Ok(());
    }

    if check_only {
        if info.update_available {
            println!("Update available: v{}", info.latest);
            print_whats_new(&info.current, &info.latest).await;
        } else if let Some(ref p) = pin {
            println!("Pin requested: {p}");
        }
        // exit 1 = update available (useful for scripting)
        std::process::exit(1);
    }

    // ── Resolve the target tag ────────────────────────────────────────────────
    let target_tag = pin.as_deref().unwrap_or(&info.latest_tag);
    let target_ver = target_tag.trim_start_matches('v');

    // Show every version's notes between installed and target before asking to confirm.
    print_whats_new(&info.current, target_ver).await;

    // ── Confirm ───────────────────────────────────────────────────────────────
    if !yes {
        use std::io::IsTerminal as _;
        if std::io::stdin().is_terminal() {
            print!("\n  Update v{} → v{}? [y/N] ", info.current, target_ver);
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if input.trim().to_lowercase() != "y" {
                println!("Aborted.");
                return Ok(());
            }
        }
    }

    // ── Apply ─────────────────────────────────────────────────────────────────
    println!("\n  Downloading v{target_ver}…");
    let applied = indexa_update::apply(target_tag).await?;
    println!("  ✓ Updated to v{applied}.");
    println!("  Restart indexa to use the new version.");

    Ok(())
}

/// Print the cumulative changelog for the versions gained by updating `from` → `to`
/// (the same span the desktop app shows). Fail-open: any fetch/parse problem is silent,
/// so the update flow never breaks over a changelog hiccup.
async fn print_whats_new(from: &str, to: &str) {
    if let Ok(notes) = indexa_update::cumulative_notes(from, to).await {
        let notes = notes.trim();
        if !notes.is_empty() {
            println!("\n  What's new:\n{notes}");
        }
    }
}
