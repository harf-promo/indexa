// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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

    // Tell the embedded web server that it is running inside the desktop app:
    //   INDEXA_DESKTOP=1    — enables `relaunch:"desktop"` in the update handler
    //   INDEXA_WEB_ALLOW_UPDATE=1 — un-gates the POST /api/update/apply endpoint
    //
    // Safety: set before any threads read these vars.
    #[allow(unused_unsafe)] // stable Rust pre-1.80 doesn't need unsafe; 1.80+ does
    unsafe {
        std::env::set_var("INDEXA_DESKTOP", "1");
        std::env::set_var("INDEXA_WEB_ALLOW_UPDATE", "1");
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            use tauri::{
                menu::{Menu, MenuItem},
                tray::TrayIconBuilder,
            };
            let show   = MenuItem::with_id(app, "show",         "Show Indexa",         true, None::<&str>)?;
            let update = MenuItem::with_id(app, "check-update", "Check for Updates…",  true, None::<&str>)?;
            let quit   = MenuItem::with_id(app, "quit",         "Quit",                true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &update, &quit])?;

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
                    "check-update" => run_update_check(app.clone()),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

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

            // Kick off a background update check on every launch.
            run_update_check(app.handle().clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Indexa desktop app");
}

/// Spawn an async update check. If a newer release is available it downloads and
/// installs it, then shows a native dialog asking the user to restart.
fn run_update_check(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let updater = match app.updater() {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[indexa-desktop] updater init failed: {e:#}");
                return;
            }
        };
        let update = match updater.check().await {
            Ok(Some(u)) => u,
            Ok(None) => return, // already up to date
            Err(e) => {
                // Network errors (offline, GitHub unavailable) are expected.
                eprintln!("[indexa-desktop] update check skipped: {e:#}");
                return;
            }
        };

        let version = update.version.clone();
        eprintln!("[indexa-desktop] update available: {version} — downloading…");

        if let Err(e) = update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
        {
            eprintln!("[indexa-desktop] update install failed: {e:#}");
            return;
        }

        // macOS 26+ Code Signing Monitor invalidates the trust record when the .app bundle
        // is overwritten in place — even with an identical ad-hoc signature — so the freshly
        // installed app would be killed on launch (exit 137). Re-sign the bundle before we
        // restart into it. Mirrors the CLI's `indexa update` fix (crates/update/src/lib.rs);
        // non-fatal so a missing/older `codesign` never blocks the update.
        #[cfg(target_os = "macos")]
        resign_app_bundle();

        eprintln!("[indexa-desktop] update installed — prompting restart");

        // Ask the user before restarting so they aren't surprised.
        let msg = format!(
            "Indexa {version} has been installed.\n\nRestart now to apply the update?"
        );
        let wants_restart = show_confirm_dialog("Indexa Update Ready", &msg);
        if wants_restart {
            app.restart();
        }
    });
}

/// Re-sign the running `.app` bundle with an ad-hoc signature after an in-place update,
/// so macOS 26+'s Code Signing Monitor will let the replaced binary launch. The bundle is
/// `<exe>/../../..` (exe = `Indexa.app/Contents/MacOS/indexa-desktop`). `--deep` covers the
/// nested frameworks. Failures only warn — `codesign` ships with Xcode Command Line Tools and
/// a missing one must not block the update flow.
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

    let embedder: Arc<dyn indexa_embed::Embedder + Send + Sync + 'static> = Arc::from(
        indexa_embed::from_config_with_keep_alive(
            &cfg.embedding.provider,
            &cfg.embedding.model,
            cfg.embedding.dim,
            &cfg.embedding.base_url,
            cfg.api_keys.openai.as_deref(),
            cfg.api_keys.google.as_deref(),
            Some(keep_alive),
            cfg.describer.num_ctx,
        )?,
    );

    let llm: Arc<dyn indexa_llm::Generator + Send + Sync + 'static> = Arc::from(
        indexa_llm::from_config_with_keep_alive(
            &cfg.describer.provider,
            &cfg.describer.model,
            &cfg.describer.base_url,
            cfg.api_keys.openai.as_deref(),
            cfg.api_keys.anthropic.as_deref(),
            Some(keep_alive),
            cfg.describer.num_ctx,
        )?,
    );

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

/// Show a native OS confirmation dialog. Returns `true` if the user clicks the
/// primary (OK/Yes) button. Falls back to `true` on unsupported platforms.
fn show_confirm_dialog(title: &str, message: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "display alert {title:?} message {message:?} \
                 buttons {{\"Later\", \"Restart Now\"}} default button \"Restart Now\""
            ))
            .output();
        match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains("Restart Now"),
            Err(_) => true, // can't show dialog → restart anyway
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, message);
        true
    }
}
