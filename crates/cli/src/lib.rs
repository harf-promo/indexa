use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "indexa",
    version,
    about = "The first tool to give your computer a memory.",
    long_about = None,
)]
pub struct Cli {
    /// Path to config file (default: platform config dir / config.toml).
    #[arg(long, global = true)]
    pub config: Option<String>,

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

        /// Embedding model to use (overrides config).
        #[arg(long)]
        embed_model: Option<String>,
    },

    /// Ask a question about your indexed files.
    Ask {
        /// Natural-language question.
        question: String,

        /// Embedding model (must match what was used during indexing; overrides config).
        #[arg(long)]
        embed_model: Option<String>,

        /// Generation model for answer synthesis (overrides config).
        #[arg(long)]
        llm_model: Option<String>,
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

    #[test]
    fn cli_ask_without_model_flags() {
        let cli = Cli::try_parse_from(["indexa", "ask", "where are my tax docs?"]).unwrap();
        match cli.command {
            Commands::Ask {
                question,
                embed_model,
                llm_model,
            } => {
                assert_eq!(question, "where are my tax docs?");
                assert!(embed_model.is_none());
                assert!(llm_model.is_none());
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_ask_with_model_flags() {
        let cli = Cli::try_parse_from([
            "indexa",
            "ask",
            "query",
            "--embed-model",
            "nomic-embed-text:v1.5",
            "--llm-model",
            "llama3.2:8b",
        ])
        .unwrap();
        match cli.command {
            Commands::Ask {
                embed_model,
                llm_model,
                ..
            } => {
                assert_eq!(embed_model.as_deref(), Some("nomic-embed-text:v1.5"));
                assert_eq!(llm_model.as_deref(), Some("llama3.2:8b"));
            }
            _ => panic!("wrong command"),
        }
    }
}
