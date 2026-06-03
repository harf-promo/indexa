use anyhow::Result;
use clap::Parser;
use indexa_cli::{Cli, Commands};
use indexa_core::config;
use tracing_subscriber::prelude::*;

mod commands;

#[tokio::main]
async fn main() -> Result<()> {
    // Determine log directory: <data_dir>/logs/ or a fallback temp path.
    let log_dir = indexa_core::config::default_data_dir()
        .map(|d| d.join("logs"))
        .unwrap_or_else(|| std::env::temp_dir().join("indexa-logs"));
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "indexa.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(tracing::Level::INFO.into());

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(env_filter.clone()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_filter(env_filter),
        )
        .init();

    // Panic hook: capture backtraces to the log file before crashing.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<unknown>");
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown location>".to_owned());
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = msg, location = %location, backtrace = %bt, "indexa panicked");
    }));

    let cli = Cli::parse();

    let cfg = if let Some(path) = &cli.config {
        let expanded = shellexpand::tilde(path).into_owned();
        config::load(std::path::Path::new(&expanded))?
    } else {
        config::load_default()?
    };

    match cli.command {
        Commands::Scan { paths, all } => commands::cmd_scan(paths, all).await,
        Commands::Deep {
            paths,
            embed_model,
            dry_run,
            mode,
        } => commands::cmd_deep(paths, embed_model, dry_run, mode, &cfg).await,
        Commands::Map { depth } => commands::cmd_map(depth).await,
        Commands::Summarize {
            paths,
            mode,
            passes,
        } => commands::cmd_summarize(paths, mode, passes, &cfg).await,
        Commands::Describe { path } => commands::cmd_describe(path).await,
        Commands::Worker { concurrency } => commands::cmd_worker(concurrency, &cfg).await,
        Commands::Export {
            paths,
            format,
            depth,
            output,
        } => commands::cmd_export(paths, format, depth, output).await,
        Commands::Ask {
            question,
            embed_model,
            llm_model,
            scope,
            top_k,
            sparse_only,
            dense_only,
        } => {
            commands::cmd_ask(
                question,
                embed_model,
                llm_model,
                scope,
                top_k,
                sparse_only,
                dense_only,
                &cfg,
            )
            .await
        }
        Commands::Watch { paths, embed_model } => {
            commands::cmd_watch(paths, embed_model, &cfg).await
        }
        Commands::Serve {
            port,
            embed_model,
            llm_model,
        } => commands::cmd_serve(port, embed_model, llm_model, &cfg).await,
        Commands::Mcp {} => commands::cmd_mcp(&cfg).await,
        Commands::Status { unknown } => commands::cmd_status(unknown, &cfg).await,
        Commands::Rm { paths, recursive } => commands::cmd_rm(paths, recursive).await,
        Commands::Doctor {
            profile,
            files,
            chunks,
        } => commands::cmd_doctor(profile, files, chunks).await,
        Commands::Fingerprint { paths } => commands::cmd_fingerprint(paths).await,
        Commands::Classify { paths, category } => commands::cmd_classify(paths, category).await,
        Commands::Update { check, yes, pin } => commands::cmd_update(check, yes, pin).await,
    }
}
