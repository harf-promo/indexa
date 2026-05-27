use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "indexa",
    version,
    about = "The first tool to give your computer a memory.",
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
        /// Paths to scan. Omit to scan the home directory.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Scan the entire computer (uses two-phase surface + deep scan).
        #[arg(long, conflicts_with = "paths")]
        all: bool,
    },

    /// Print a summary map of what Indexa found and how regions were classified.
    Map,

    /// Deep-scan a path: parse, embed, and index file contents.
    Deep {
        /// Path to deep-scan. Omit to deep-scan the entire existing index.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Ollama embedding model to use.
        #[arg(long, default_value = "nomic-embed-text")]
        embed_model: String,
    },

    /// Ask a question about your indexed files.
    Ask {
        /// Natural-language question.
        question: String,

        /// Ollama embedding model (must match what was used during indexing).
        #[arg(long, default_value = "nomic-embed-text")]
        embed_model: String,

        /// Ollama generation model.
        #[arg(long, default_value = "qwen2.5:14b")]
        llm_model: String,
    },

    /// Start the background watcher daemon (keeps the index current).
    Watch,

    /// Start the local web UI at http://localhost:<port>.
    Serve {
        #[arg(short, long, default_value_t = 7620)]
        port: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_scan_path() {
        let cli = Cli::try_parse_from(["indexa", "scan", "~/Documents"]).unwrap();
        match cli.command {
            Commands::Scan { paths, all } => {
                assert_eq!(paths, vec!["~/Documents"]);
                assert!(!all);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_parses_scan_all() {
        let cli = Cli::try_parse_from(["indexa", "scan", "--all"]).unwrap();
        match cli.command {
            Commands::Scan { paths, all } => {
                assert!(paths.is_empty());
                assert!(all);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_help_doesnt_panic() {
        Cli::command().debug_assert();
    }
}
