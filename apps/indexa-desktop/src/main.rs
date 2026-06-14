// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use indexa_web::{report_update_progress, wait_for_update_command, UpdateCommand, UpdateProgress};
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

const PORT: u16 = 7620;

fn main() {
    // Initialise tracing so the embedded server can log to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "warn,indexa_web=info".to_owned())
                .as_str(),
        )
        .with_writer(std::io::stderr)
        .init();

    // Guard against port conflicts: if something is already listening on PORT
    // before we spawn our server, we refuse to start rather than silently
    // attaching the webview to a foreign service.
    if std::net::TcpStream::connect(format!("127.0.0.1:{PORT}")).is_ok() {
        let msg = format!(
            "Port {PORT} is already in use.\n\n\
             Another `indexa serve` process may be running. \
             Quit it and relaunch Indexa."
        );
        eprintln!("[indexa-desktop] {msg}");
        show_error_dialog("Indexa — Port Conflict", &msg);
        std::process::exit(1);
    }

    // Tell the embedded web server it is running inside the desktop app. The
    // desktop updates ONLY through the Tauri native updater (menu-bar "Check for
    // Updates…"), which installs the notarized `.app.tar.gz` as a whole bundle.
    //
    // We deliberately do NOT set INDEXA_WEB_ALLOW_UPDATE here. That gate un-locks
    // the web "Update now" button → `indexa_update::apply()`, the CLI's *binary*
    // self-replace: it downloads the headless `indexa-<arch>-apple-darwin` CLI
    // binary and renames it over `Contents/MacOS/indexa-desktop`, then ad-hoc
    // re-signs it — stripping Developer-ID + notarization and bricking the app
    // (Gatekeeper then refuses to launch the quarantined ad-hoc bundle). The web
    // apply path additionally refuses when INDEXA_DESKTOP=1, and the updater
    // itself refuses to self-replace inside a `.app` — three independent guards.
    //
    // Safety: set before any threads read this var.
    #[allow(unused_unsafe)] // stable Rust pre-1.80 doesn't need unsafe; 1.80+ does
    unsafe {
        std::env::set_var("INDEXA_DESKTOP", "1");
    }

    // Start the embedded web server on a background Tokio runtime so Tauri's
    // main thread stays free for the UI event loop.
    std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            if let Err(e) = run_server(PORT).await {
                eprintln!("[indexa-desktop] server error: {e:#}");
                std::process::exit(1); // propagate fatal errors (e.g. bind failure) to the UI process
            }
        });
    });

    // Wait until our server is accepting connections (up to 15 s).
    if !wait_for_port(PORT, Duration::from_secs(15)) {
        eprintln!("[indexa-desktop] server did not start within 15 s");
        std::process::exit(1);
    }

    // Build the Tauri application.
    tauri::Builder::default()
        .plugin({
            // Release builds ship a single universal (Intel + Apple-Silicon) bundle.
            // tauri-action publishes it under BOTH per-arch updater keys in latest.json
            // (`darwin-aarch64` + `darwin-x86_64`), each pointing at the same universal
            // artifact — so the DEFAULT per-arch updater target resolves correctly on
            // both architectures. Do NOT pin `.target("darwin-universal")`: tauri never
            // emits that key, so pinning it makes the updater find no update. Mirrors the
            // `--target universal-apple-darwin` build in .github/workflows/release.yml.
            tauri_plugin_updater::Builder::new().build()
        })
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            use tauri::{
                menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu},
                tray::TrayIconBuilder,
            };
            let show = MenuItem::with_id(app, "show", "Show Indexa", true, None::<&str>)?;
            let update = MenuItem::with_id(
                app,
                "check-update",
                "Check for Updates…",
                true,
                None::<&str>,
            )?;
            let install_cli = MenuItem::with_id(
                app,
                "install-cli",
                "Install command-line tool",
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &update, &install_cli, &quit])?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    "check-update" => run_update_check(app.clone(), true),
                    "install-cli" => run_cli_install(app.clone()),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Native macOS menu bar so "Check for Updates" lives where macOS users look
            // (the app menu), not only in the tray icon. Also gives a standard ⌘Q, an
            // About box, and working ⌘C/⌘X/⌘V in the webview's text fields. Uses a distinct
            // id ("app-check-update") from the tray's "check-update" so neither double-fires.
            let app_check = MenuItem::with_id(
                app,
                "app-check-update",
                "Check for Updates…",
                true,
                None::<&str>,
            )?;
            let app_install_cli = MenuItem::with_id(
                app,
                "app-install-cli",
                "Install command-line tool",
                true,
                None::<&str>,
            )?;
            let about_meta = AboutMetadata {
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                ..Default::default()
            };
            let app_menu = Submenu::with_items(
                app,
                "Indexa",
                true,
                &[
                    &PredefinedMenuItem::about(app, Some("Indexa"), Some(about_meta))?,
                    &PredefinedMenuItem::separator(app)?,
                    &app_check,
                    &app_install_cli,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::show_all(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?;
            let edit_menu = Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?;
            let window_menu = Submenu::with_items(
                app,
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(app, None)?,
                    &PredefinedMenuItem::close_window(app, None)?,
                ],
            )?;
            let menu_bar = Menu::with_items(app, &[&app_menu, &edit_menu, &window_menu])?;
            app.set_menu(menu_bar)?;
            app.on_menu_event(|app, event| match event.id().as_ref() {
                "app-check-update" => run_update_check(app.clone(), true),
                "app-install-cli" => run_cli_install(app.clone()),
                _ => {}
            });

            // Hide the window instead of closing when the user clicks ✕.
            // The app stays alive in the menu bar; "Quit" in the tray menu exits.
            if let Some(window) = app.get_webview_window("main") {
                let win = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = win.hide();
                    }
                });
            }

            // Auto check-on-launch is CHECK-ONLY (manual=false): it never silently
            // downloads + installs, so reopening Indexa no longer surprises you with a
            // restart prompt. To actually install, use "Check for Updates…" (app menu
            // or tray) — the web Settings → Software Update panel points there too.
            run_update_check(app.handle().clone(), false);

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Indexa desktop app")
        .run(|app_handle, event| {
            // Re-show the window when the Dock icon is clicked after the window was closed
            // (close hides to the tray) — without this the app is unreachable from the Dock.
            // `RunEvent::Reopen` is a macOS-only variant (Dock reopen), so the arm must be
            // gated or the closure fails to compile on Linux/Windows.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(win) = app_handle.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app_handle, event); // Reopen doesn't exist off macOS; nothing to do.
        });
}

/// Spawn an async update check.
///
/// `manual = true` (the user chose "Check for Updates…" from the app menu or tray) runs the
/// full flow — check → download → install → re-sign → confirm restart — and reports "you're
/// up to date" / errors in a dialog. `manual = false` (the automatic check on every launch)
/// is **check-only**: it never downloads, so reopening Indexa can't surprise the user with a
/// restart prompt (the bug behind "it tells me to update when I reopen"). An available update
/// is still surfaced in the web Settings → Software Update panel via `/api/update/check`.
fn run_update_check(app: tauri::AppHandle, manual: bool) {
    tauri::async_runtime::spawn(async move {
        let updater = match app.updater() {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[indexa-desktop] updater init failed: {e:#}");
                if manual {
                    show_info_dialog("Indexa Update", "Could not start the updater.");
                }
                return;
            }
        };
        match updater.check().await {
            Ok(None) => {
                if manual {
                    show_info_dialog("Indexa", "You're on the latest version.");
                }
            }
            Ok(Some(update)) => {
                if manual {
                    install_update(app, update).await;
                } else {
                    eprintln!(
                        "[indexa-desktop] update {} available — use “Check for Updates…” to install",
                        update.version
                    );
                }
            }
            Err(e) => {
                // Offline / GitHub unavailable is expected; only surface it on a manual check.
                eprintln!("[indexa-desktop] update check skipped: {e:#}");
                if manual {
                    show_info_dialog(
                        "Indexa Update",
                        "Couldn't check for updates — check your connection and try again.",
                    );
                }
            }
        }
    });
}

/// Show the in-app changelog window, wait for the user's choice, then (on Install) download,
/// re-sign (macOS), and restart. Replaces the old blocking osascript confirm — the whole flow is
/// now in-app (changelog modal → progress bar → restart), bridged over the embedded server's SSE.
async fn install_update(app: tauri::AppHandle, update: tauri_plugin_updater::Update) {
    use std::sync::atomic::{AtomicBool, Ordering};
    // The tray item and the macOS app-menu item both route to run_update_check → install_update;
    // guard so a double-trigger can't open two flows or download twice.
    static INSTALL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
    if INSTALL_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return;
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            INSTALL_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
    let _guard = Guard;

    let version = update.version.clone();
    let title = format!("Indexa {version}");

    // Publish the FULL changelog to the webview, which renders an in-app "update available" modal
    // (white card, scrollable notes, Install & Relaunch / Later) — no native dialog. Bring the
    // window forward so it's visible. The webview replies via POST /api/update/control, waking
    // `wait_for_update_command` below. (The webview loads a remote URL with no Tauri IPC, so both
    // directions are bridged through the embedded server.)
    report_update_progress(UpdateProgress::available(version.clone(), update.body.clone()));
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }

    // Wait for the user's choice with a generous timeout, so a walked-away user never triggers a
    // surprise download. Dismiss or timeout → reset to idle and stop.
    match tokio::time::timeout(Duration::from_secs(600), wait_for_update_command()).await {
        Ok(UpdateCommand::Start) => { /* fall through to download */ }
        Ok(UpdateCommand::Dismiss) | Err(_) => {
            report_update_progress(UpdateProgress::idle());
            eprintln!("[indexa-desktop] update {version} dismissed (or timed out)");
            return;
        }
    }

    report_update_progress(UpdateProgress::downloading(title.clone(), 0, None));

    eprintln!("[indexa-desktop] update {version} — downloading…");
    // `download_and_install`'s on_chunk closure is `Fn` (not `FnMut`), so accumulate the running
    // byte count through an atomic. chunk_len is per-chunk; total is the Content-Length if present.
    let downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dl = downloaded.clone();
    let title_chunk = title.clone();
    let title_finish = title.clone();
    if let Err(e) = update
        .download_and_install(
            move |chunk_len, total| {
                let n = dl.fetch_add(chunk_len as u64, std::sync::atomic::Ordering::Relaxed)
                    + chunk_len as u64;
                report_update_progress(UpdateProgress::downloading(title_chunk.clone(), n, total));
            },
            move || report_update_progress(UpdateProgress::installing(title_finish.clone())),
        )
        .await
    {
        eprintln!("[indexa-desktop] update install failed: {e:#}");
        report_update_progress(UpdateProgress::error(title.clone(), format!("{e}")));
        show_info_dialog(
            "Indexa Update",
            &format!("The update failed to install: {e}"),
        );
        return;
    }

    // macOS 26+ Code Signing Monitor invalidates the trust record when the .app bundle
    // is overwritten in place — even with an identical ad-hoc signature — so the freshly
    // installed app would be killed on launch (exit 137). Re-sign the bundle before we
    // restart into it. Mirrors the CLI's `indexa update` fix (crates/update/src/lib.rs);
    // non-fatal so a missing/older `codesign` never blocks the update.
    #[cfg(target_os = "macos")]
    resign_app_bundle();

    report_update_progress(UpdateProgress::done(title));
    eprintln!("[indexa-desktop] update {version} installed — restarting");
    // The user clicked "Install & Relaunch" in the in-app modal, so restart straight into the new
    // version.
    app.restart();
}

/// Download the matching `indexa` CLI binary from this release into a PATH directory, so the
/// owner's terminal `indexa` command tracks the desktop app's version (their CLI was stuck at
/// v0.19 while the app moved on). Runs off the UI thread; reports success/failure in a dialog.
///
/// Target directory (best-effort, in priority order):
///   1. the dir of an existing `indexa` already on `$PATH` — overwrites the binary the user's
///      shell actually resolves, so `indexa` updates in place;
///   2. else `~/.cargo/bin` if it exists (common rustup/cargo install location);
///   3. else `~/.local/bin` (created if missing; the standard user-bin dir).
fn run_cli_install(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        // Surface the in-app progress overlay (same SSE-driven bar as the app self-update).
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.show();
            let _ = win.set_focus();
        }

        let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        let (dir, on_path) = resolve_cli_dir();
        eprintln!(
            "[indexa-desktop] installing CLI {tag} → {} (on PATH: {on_path})",
            dir.display()
        );

        let cli_title = "Command-line tool";
        report_update_progress(UpdateProgress::downloading(cli_title, 0, None));
        // download_cli_to passes (cumulative downloaded, total) per chunk; bridge it to the bar.
        let on_progress = move |downloaded: u64, total: Option<u64>| {
            report_update_progress(UpdateProgress::downloading(cli_title, downloaded, total));
        };
        match indexa_update::download_cli_to(&dir, &tag, Some(&on_progress)).await {
            Ok(path) => {
                report_update_progress(UpdateProgress::done(cli_title));
                let mut msg = format!(
                    "The indexa command-line tool ({tag}) was installed to:\n{}",
                    path.display()
                );
                if on_path {
                    msg.push_str(
                        "\n\nOpen a new terminal and run `indexa --version` to confirm \
                         it matches this app.",
                    );
                } else {
                    // The GUI app's $PATH is the minimal launchd one, not your shell's, so we
                    // can't reliably tell whether this folder is on your *terminal* PATH — phrase
                    // it as a conditional rather than asserting it isn't.
                    msg.push_str(&format!(
                        "\n\nIf `indexa` isn't found in a new terminal, add this folder to your \
                         PATH, e.g.:\n\
                         echo 'export PATH=\"{}:$PATH\"' >> ~/.zshrc\n\
                         then open a new terminal and run `indexa --version`.",
                        dir.display()
                    ));
                }
                eprintln!("[indexa-desktop] CLI installed to {}", path.display());
                show_info_dialog("Indexa CLI installed", &msg);
            }
            Err(e) => {
                eprintln!("[indexa-desktop] CLI install failed: {e:#}");
                report_update_progress(UpdateProgress::error(cli_title, format!("{e}")));
                show_info_dialog(
                    "Indexa CLI install failed",
                    &format!("Couldn't install the command-line tool:\n{e}"),
                );
            }
        }
    });
}

/// Resolve where to install the `indexa` CLI. Returns `(dir, already_on_path)`.
///
/// Prefers overwriting an `indexa` already resolvable on `$PATH` (so the user's existing command
/// updates in place); otherwise falls back to `~/.cargo/bin` (if present) or `~/.local/bin`.
fn resolve_cli_dir() -> (std::path::PathBuf, bool) {
    let bin_name = if cfg!(windows) {
        "indexa.exe"
    } else {
        "indexa"
    };

    // 1) An existing `indexa` on PATH — overwrite it in place.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if dir.join(bin_name).is_file() {
                return (dir, true);
            }
        }
    }

    // 2)/3) Fall back to a user-writable bin dir under HOME.
    let home = home_dir();
    if let Some(home) = home.as_ref() {
        let cargo_bin = home.join(".cargo").join("bin");
        if cargo_bin.is_dir() {
            let on_path = dir_on_path(&cargo_bin);
            return (cargo_bin, on_path);
        }
        let local_bin = home.join(".local").join("bin");
        let on_path = dir_on_path(&local_bin);
        return (local_bin, on_path);
    }

    // No HOME (unusual): use the current dir, reported as not-on-PATH.
    (std::path::PathBuf::from("."), false)
}

/// Best-effort home directory without pulling in the `dirs` crate.
fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }
}

/// Is `dir` one of the entries in `$PATH`?
fn dir_on_path(dir: &std::path::Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}

/// Re-sign the running `.app` bundle with an **ad-hoc** signature after an in-place update,
/// so macOS 26+'s Code Signing Monitor will let the replaced binary launch. The bundle is
/// `<exe>/../../..` (exe = `Indexa.app/Contents/MacOS/indexa-desktop`). `--deep` covers the
/// nested frameworks. Failures only warn — `codesign` ships with Xcode Command Line Tools and
/// a missing one must not block the update flow.
///
/// Self-disables when the freshly-installed bundle already carries a real **Developer ID
/// Application** signature: re-signing ad-hoc would strip that (and its notarization), so once
/// proper signing goes live (v0.20+ release builds) this becomes a no-op automatically — no
/// version coordination needed.
#[cfg(target_os = "macos")]
fn resign_app_bundle() {
    let Some(bundle) = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|exe| {
            exe.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
        })
    else {
        eprintln!("[indexa-desktop] codesign skipped: cannot resolve app bundle path");
        return;
    };
    let bundle_str = bundle.to_string_lossy();

    // FAIL CLOSED. Ad-hoc re-signing a Developer-ID + notarized bundle strips the
    // notarization and bricks the app, so we only ever re-sign when we can
    // POSITIVELY confirm the installed bundle is ad-hoc/linker-signed with no
    // Developer-ID authority. Any probe failure, or output we don't recognize, is
    // treated as "might be notarized" → leave it untouched. (Release builds are
    // notarized since v0.20, so on a real install this is always a no-op; the
    // ad-hoc branch only fires for unsigned dev/CI fallback bundles.)
    let probe = std::process::Command::new("codesign")
        .args(["-dvv", bundle_str.as_ref()])
        .output();
    let is_confirmed_adhoc = match probe {
        Ok(info) => {
            let out = format!(
                "{}{}",
                String::from_utf8_lossy(&info.stderr),
                String::from_utf8_lossy(&info.stdout)
            );
            // Must NOT carry a Developer-ID authority, and must positively report
            // an ad-hoc/linker signature — otherwise we don't know, so don't touch it.
            !out.contains("Authority=Developer ID Application")
                && (out.contains("Signature=adhoc") || out.contains("linker-signed"))
        }
        Err(_) => false, // can't probe → assume notarized → leave it alone
    };
    if !is_confirmed_adhoc {
        eprintln!(
            "[indexa-desktop] bundle is Developer-ID/notarized (or signature unverifiable) \
             — skipping ad-hoc re-sign"
        );
        return;
    }

    match std::process::Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-", bundle_str.as_ref()])
        .output()
    {
        Ok(out) if out.status.success() => {
            eprintln!("[indexa-desktop] re-signed {bundle_str} after update");
        }
        Ok(out) => {
            eprintln!(
                "[indexa-desktop] codesign re-sign failed ({:?}): {} — the updated app may not \
                 launch; run: codesign --force --deep --sign - {bundle_str}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!(
                "[indexa-desktop] could not run codesign after update: {e} — install Xcode \
                 Command Line Tools if the updated app fails to launch"
            );
        }
    }
}

/// Embed and start the indexa web server directly — no subprocess needed.
///
/// Reads the standard config file and index DB; errors (e.g. missing DB) are
/// logged to stderr so the window still opens and shows the "no context yet" state.
async fn run_server(port: u16) -> anyhow::Result<()> {
    use indexa_core::config;

    let config_path = config::default_config_path();
    let cfg = config::load(&config_path)?;

    let data_dir = config::default_data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    let db_path = data_dir.join("index.db");

    // Open (or create) the index DB — migrations run on open.
    let store = indexa_core::store::Store::open(&db_path)?;

    let keep_alive = cfg.resource.effective_keep_alive_secs();

    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> =
        Arc::from(indexa_embed::from_config_with_keep_alive(
            &cfg.embedding.provider,
            &cfg.embedding.model,
            cfg.embedding.dim,
            &cfg.embedding.base_url,
            cfg.api_keys.openai.as_deref(),
            cfg.api_keys.google.as_deref(),
            Some(keep_alive),
            cfg.describer.num_ctx,
        )?);

    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> =
        Arc::from(indexa_llm::from_config_with_keep_alive(
            &cfg.describer.provider,
            &cfg.describer.model,
            &cfg.describer.base_url,
            cfg.api_keys.openai.as_deref(),
            cfg.api_keys.anthropic.as_deref(),
            Some(keep_alive),
            cfg.describer.num_ctx,
        )?);

    // Desktop always binds to localhost — never expose on LAN without explicit CLI opt-in.
    indexa_web::serve(port, "127.0.0.1", store, embedder, llm, cfg).await
}

/// Poll `127.0.0.1:port` until a TCP connection succeeds or `timeout` elapses.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let start = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Show a native OS error alert. Falls back to a no-op on unsupported platforms.
fn show_error_dialog(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "display alert {title:?} message {message:?} as critical \
                 buttons {{\"OK\"}} default button \"OK\""
            ))
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, message); // suppress unused warnings
    }
}

/// Show a native OS informational alert with a single OK button. No-op off macOS.
fn show_info_dialog(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "display alert {title:?} message {message:?} \
                 buttons {{\"OK\"}} default button \"OK\""
            ))
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, message);
    }
}

