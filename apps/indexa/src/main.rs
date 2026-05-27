use anyhow::Result;
use clap::Parser;
use indexa_cli::{Cli, Commands};

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
        Commands::Scan { path } => {
            println!("Scanning: {path}");
            println!("(indexer not yet implemented — coming in v0.0.1)");
        }
        Commands::Ask { question } => {
            println!("Question: {question}");
            println!("(query engine not yet implemented — coming in v0.1)");
        }
        Commands::Watch => {
            println!("Watcher not yet implemented — coming in v0.1");
        }
        Commands::Serve { port } => {
            println!("Web UI not yet implemented — coming in v0.1");
            println!("Will serve on http://localhost:{port}");
        }
    }

    Ok(())
}
