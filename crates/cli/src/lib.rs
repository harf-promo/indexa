//! CLI command surface for Indexa — scan, ask, watch, serve.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "indexa",
    version,
    about = "The open index for your whole computer.",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Walk a path and build (or update) the index.
    Scan {
        /// Path to scan (default: home directory).
        #[arg(default_value = "~")]
        path: String,
    },
    /// Ask a question about indexed files.
    Ask {
        /// Natural-language question.
        question: String,
    },
    /// Start the background watcher daemon.
    Watch,
    /// Start the local web UI.
    Serve {
        /// Port to listen on.
        #[arg(short, long, default_value_t = 7620)]
        port: u16,
    },
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        assert_eq!(2 + 2, 4);
    }
}
