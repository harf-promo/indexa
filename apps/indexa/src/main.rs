use anyhow::Result;
use clap::Parser;
use directories::BaseDirs;
use indexa_cli::{Cli, Commands};
use indexa_core::{
    store::Store,
    walker::{walk, WalkConfig},
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Scan { paths, all } => cmd_scan(paths, all).await,
        Commands::Map => cmd_map().await,
        Commands::Ask { question } => {
            println!("Question: {question}");
            println!("(query engine not yet implemented — coming in v0.1)");
            Ok(())
        }
        Commands::Watch => {
            println!("Watcher not yet implemented — coming in v0.1");
            Ok(())
        }
        Commands::Serve { port } => {
            println!("Web UI not yet implemented — coming in v0.1");
            println!("Will serve on http://localhost:{port}");
            Ok(())
        }
    }
}

async fn cmd_scan(paths: Vec<String>, all: bool) -> Result<()> {
    let roots = resolve_roots(paths, all)?;
    let db_path = index_db_path()?;
    let mut store = Store::open(&db_path)?;

    for root in &roots {
        println!("Scanning {}", root.display());
        let entries = walk(root, &WalkConfig::default())?;
        let count = entries.len();
        store.upsert_entries(&entries)?;
        println!("  indexed {count} entries");
    }

    println!("\nIndex saved to {}", db_path.display());
    println!("Run `indexa map` to see a summary.");
    Ok(())
}

async fn cmd_map() -> Result<()> {
    let db_path = index_db_path()?;
    if !db_path.exists() {
        println!("No index found. Run `indexa scan <path>` first.");
        return Ok(());
    }

    let store = Store::open(&db_path)?;
    let total = store.entry_count()?;
    let summary = store.region_summary()?;

    println!("Indexa map — {} entries total\n", total);
    println!("{:<20} {:>10} {:>14}", "Category", "Files", "Size");
    println!("{}", "-".repeat(46));
    for r in summary {
        println!(
            "{:<20} {:>10} {:>14}",
            r.category,
            r.entry_count,
            format_size(r.total_size)
        );
    }
    Ok(())
}

fn resolve_roots(paths: Vec<String>, all: bool) -> Result<Vec<PathBuf>> {
    if all {
        // On macOS/Linux start from /, on Windows from each drive root.
        #[cfg(windows)]
        return Ok(vec![PathBuf::from("C:\\")]);
        #[cfg(not(windows))]
        return Ok(vec![PathBuf::from("/")]);
    }

    if paths.is_empty() {
        let base =
            BaseDirs::new().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        return Ok(vec![base.home_dir().to_path_buf()]);
    }

    paths
        .into_iter()
        .map(|p| {
            let expanded = shellexpand::tilde(&p).into_owned();
            Ok(PathBuf::from(expanded))
        })
        .collect()
}

fn index_db_path() -> Result<PathBuf> {
    let base =
        BaseDirs::new().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    Ok(base.data_local_dir().join("indexa").join("index.db"))
}

fn format_size(bytes: u64) -> String {
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
