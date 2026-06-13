use anyhow::Result;
use clap::Parser;
use indexa_cli::{
    Cli, Commands, InsightsAction, McpAction, PackAction, ReviewAction, SavedAction,
    SnapshotAction, WeightAction,
};
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
        Commands::Index {
            paths,
            embed_model,
            mode,
            passes,
        } => commands::cmd_index(paths, embed_model, mode, passes, &cfg).await,
        Commands::Scan { paths, all } => commands::cmd_scan(paths, all, &cfg).await,
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
        Commands::Worker {
            concurrency,
            auto_reindex,
        } => commands::cmd_worker(concurrency, auto_reindex, &cfg).await,
        Commands::Pack { action } => match action {
            PackAction::Create {
                name,
                description,
                auto,
                yes,
                limit,
            } => commands::cmd_pack_create(name, description, auto, yes, limit, &cfg).await,
            PackAction::Add { name, paths } => commands::cmd_pack_add(name, paths).await,
            PackAction::Remove { name, paths } => commands::cmd_pack_remove(name, paths).await,
            PackAction::List => commands::cmd_pack_list().await,
            PackAction::Show { name } => commands::cmd_pack_show(name).await,
            PackAction::Export {
                name,
                format,
                output,
                depth,
                include_weights,
            } => commands::cmd_pack_export(name, format, output, depth, include_weights).await,
            PackAction::Rename { name, new_name } => {
                commands::cmd_pack_rename(name, new_name).await
            }
            PackAction::Delete { name } => commands::cmd_pack_delete(name).await,
        },
        Commands::Weight { action } => match action {
            WeightAction::Set {
                target,
                weight,
                kind,
            } => commands::cmd_weight_set(target, weight, kind, &cfg).await,
            WeightAction::Get { path } => commands::cmd_weight_get(path).await,
            WeightAction::List { kind } => commands::cmd_weight_list(kind).await,
            WeightAction::Delete { target, kind } => {
                commands::cmd_weight_delete(target, kind).await
            }
            WeightAction::Suggest { days } => commands::cmd_weight_suggest(days).await,
            WeightAction::Apply { days, yes } => commands::cmd_weight_apply(days, yes).await,
        },
        Commands::Insights { action } => match action {
            InsightsAction::Duplicates { threshold, exact } => {
                commands::cmd_insights_duplicates(threshold, exact).await
            }
            InsightsAction::Stale { days } => commands::cmd_insights_stale(days).await,
            InsightsAction::Diff { days } => commands::cmd_insights_diff(days).await,
            InsightsAction::Largest { limit, json } => {
                commands::cmd_insights_largest(limit, json).await
            }
            InsightsAction::Languages { json } => commands::cmd_insights_languages(json).await,
        },
        Commands::Snapshot { action } => match action {
            SnapshotAction::Export { output } => commands::cmd_snapshot_export(output).await,
            SnapshotAction::Import { file } => commands::cmd_snapshot_import(file).await,
        },
        Commands::Eval {
            golden,
            mode,
            top_k,
            scope,
            json,
            min_hit_rate,
        } => commands::cmd_eval(golden, mode, top_k, scope, json, min_hit_rate, &cfg).await,
        Commands::Report {
            questions,
            saved,
            format,
            output,
        } => commands::cmd_report(questions, saved, format, output, &cfg).await,
        Commands::Saved { action } => match action {
            SavedAction::Add {
                name,
                question,
                mode,
                scope,
            } => commands::cmd_saved_add(name, question, mode, scope).await,
            SavedAction::List { json } => commands::cmd_saved_list(json).await,
            SavedAction::Run { name, json } => commands::cmd_saved_run(name, json, &cfg).await,
            SavedAction::Rm { name } => commands::cmd_saved_rm(name).await,
        },
        Commands::Review { action } => match action {
            ReviewAction::List { decision_type } => commands::cmd_review_list(decision_type).await,
            ReviewAction::Show { id } => commands::cmd_review_show(id).await,
            ReviewAction::Answer {
                id,
                choice,
                decision_type,
                under,
                choose,
            } => commands::cmd_review_answer(id, choice, decision_type, under, choose).await,
            ReviewAction::Dismiss { id } => commands::cmd_review_dismiss(id).await,
            ReviewAction::History { path } => commands::cmd_review_history(path).await,
            ReviewAction::Revert { id } => commands::cmd_review_revert(id).await,
            ReviewAction::Scan => commands::cmd_review_scan(&cfg).await,
            ReviewAction::Gc { older_than_days } => commands::cmd_review_gc(older_than_days).await,
        },
        Commands::Graph {
            path,
            limit,
            strict,
            cycles,
        } => commands::cmd_graph(path, limit, strict, cycles).await,
        Commands::Related { path, limit, json } => commands::cmd_related(path, limit, json).await,
        Commands::Export {
            paths,
            format,
            depth,
            output,
            include_weights,
            include_graph,
        } => {
            commands::cmd_export(paths, format, depth, output, include_weights, include_graph).await
        }
        Commands::Ask {
            question,
            embed_model,
            llm_model,
            scope,
            top_k,
            sparse_only,
            dense_only,
            agentic,
            max_steps,
            explain,
            json,
        } => {
            commands::cmd_ask(
                question,
                embed_model,
                llm_model,
                scope,
                top_k,
                sparse_only,
                dense_only,
                agentic,
                max_steps,
                explain,
                json,
                &cfg,
            )
            .await
        }
        Commands::Search {
            query,
            top_k,
            scope,
            dense,
            hybrid,
            json,
        } => commands::cmd_search(query, top_k, scope, dense, hybrid, json, &cfg).await,
        Commands::Watch { paths, embed_model } => {
            commands::cmd_watch(paths, embed_model, &cfg).await
        }
        Commands::Serve {
            port,
            host,
            embed_model,
            llm_model,
        } => commands::cmd_serve(port, host, embed_model, llm_model, &cfg).await,
        // Bare `indexa mcp` must keep running the stdio server — every client
        // config written by `mcp install` points at exactly that invocation.
        Commands::Mcp { action } => match action {
            None => commands::cmd_mcp(&cfg).await,
            Some(McpAction::Install { client, dry_run }) => {
                commands::cmd_mcp_install(client, dry_run).await
            }
        },
        Commands::Status {
            unknown,
            deep,
            json,
        } => commands::cmd_status(unknown, deep, json, &cfg).await,
        Commands::Rm { paths, recursive } => commands::cmd_rm(paths, recursive).await,
        Commands::Prune { dry_run } => commands::cmd_prune(dry_run).await,
        Commands::Doctor {
            profile,
            files,
            chunks,
            apply_ollama_env,
        } => commands::cmd_doctor(profile, files, chunks, apply_ollama_env).await,
        Commands::Fingerprint { paths } => commands::cmd_fingerprint(paths).await,
        Commands::Classify { paths, category } => {
            commands::cmd_classify(paths, category, &cfg).await
        }
        Commands::Update { check, yes, pin } => commands::cmd_update(check, yes, pin).await,
        Commands::Completion { shell } => commands::cmd_completion(shell),
    }
}
