// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;

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
            }
        });
    });

    // Wait until the server is accepting connections (up to 15 s) before
    // opening the webview — avoids a blank window on slow storage or cold
    // Ollama start.
    if !wait_for_port(PORT, Duration::from_secs(15)) {
        eprintln!(
            "[indexa-desktop] warning: server did not start within 15 s; opening window anyway"
        );
    }

    // Build the Tauri application.
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            // Tray icon: pause/resume is a future addition; for now just
            // provide a "Show" and "Quit" menu so the app is quit-able from
            // the menu bar when all windows are closed.
            use tauri::{
                menu::{Menu, MenuItem},
                tray::TrayIconBuilder,
            };
            let show = MenuItem::with_id(app, "show", "Show Indexa", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

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
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Indexa desktop app");
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

    indexa_web::serve(port, store, embedder, llm, cfg).await
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
