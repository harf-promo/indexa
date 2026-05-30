use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "indexa",
    version,
    about = "The local context engine for AI — index your disk once, hand grounded context to any AI.",
    long_about = "Indexa reads your code or your disk once, builds a hierarchical context graph \
(files → summaries → folder roll-ups), and serves it to AI tools on demand.\n\n\
The index is the substrate; context is the product.\n\n\
Local-first, model-agnostic, free of token-budget tax. Fully open source.\n\n\
Quick start:\n  \
indexa scan ~/code/myrepo           # build surface context map\n  \
indexa deep ~/code/myrepo           # build deep context (parses, embeds)\n  \
indexa summarize ~/code/myrepo      # generate hierarchical summaries\n  \
indexa export ~/code/myrepo > ctx.xml  # export as XML for your AI tool\n  \
indexa serve                        # open local web UI"
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
    /// Build the surface context map of a path (fast — no AI calls).
    #[command(after_help = "Examples:
  indexa scan ~/Documents
  indexa scan ~/Projects ~/Notes
  indexa scan --all")]
    Scan {
        /// Paths to scan. Omit to scan the home directory.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Scan the entire computer (two-phase surface + deep scan).
        #[arg(long, conflicts_with = "paths")]
        all: bool,
    },

    /// Print a summary map of what Indexa found and how regions were classified.
    #[command(after_help = "Examples:
  indexa map
  indexa map --depth 2")]
    Map {
        /// Maximum depth to display (default: 3).
        #[arg(long, default_value_t = 3)]
        depth: usize,
    },

    /// Build deep context: parse, embed, and index file contents.
    ///
    /// Summarization is enqueued for background processing — run `indexa summarize`
    /// (which accepts `--passes`) or the web UI to generate the summaries.
    #[command(after_help = "Examples:
  indexa deep ~/Documents
  indexa deep ~/Projects --embed-model nomic-embed-text:v1.5
  indexa deep --dry-run ~/Documents")]
    Deep {
        /// Path to deep-scan. Omit to deep-scan the entire existing index.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Embedding model to use (overrides config).
        #[arg(long)]
        embed_model: Option<String>,

        /// Show what would be parsed/indexed without writing to the DB.
        #[arg(long)]
        dry_run: bool,

        /// Summary storage mode: augment (default), compress, summaries-only.
        #[arg(long, default_value = "augment")]
        mode: String,
    },

    /// Generate hierarchical context summaries for indexed files and directories.
    #[command(after_help = "Examples:
  indexa summarize ~/Documents
  indexa summarize ~/Documents --mode compress
  indexa summarize ~/Documents --passes 2")]
    Summarize {
        /// Path to summarize. Omit to summarize the entire existing index.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Summary mode: augment (default), compress, summaries-only.
        #[arg(long, default_value = "augment")]
        mode: String,

        /// Refinement passes per summary. Default: 2 for new context, 1 for refresh.
        /// Capped at the config `passes-cap` (default 3).
        #[arg(long)]
        passes: Option<u32>,
    },

    /// Print the summary and breadcrumb chain for a specific path.
    #[command(after_help = "Examples:
  indexa describe ~/Documents/taxes")]
    Describe {
        /// Path to describe.
        path: String,
    },

    /// Run the background summarization worker (drains the summary queue).
    #[command(after_help = "Examples:
  indexa worker
  indexa worker --concurrency 4")]
    Worker {
        /// Number of concurrent summarization tasks.
        #[arg(short, long, default_value_t = 2)]
        concurrency: usize,
    },

    /// Export the hierarchical summary tree as XML, Markdown, or JSON for use as AI context.
    #[command(after_help = "Examples:
  indexa export ~/code/myrepo --format xml > .context.xml
  indexa export ~/code/myrepo --format md
  indexa export ~/code/myrepo --format json --depth 3 --output context.json")]
    Export {
        /// Path(s) to export. Omit to export the entire index.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Output format: xml (default), md, json.
        #[arg(long, default_value = "xml")]
        format: String,

        /// Maximum tree depth (0 = root summary only).
        #[arg(long)]
        depth: Option<usize>,

        /// Write output to FILE instead of stdout.
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Query your local context with a natural-language question.
    #[command(after_help = "Examples:
  indexa ask \"where are my tax documents?\"
  indexa ask \"where is auth handled in this repo?\"
  indexa ask --scope ~/Work \"what are my current priorities?\"
  indexa ask --sparse-only \"IndexOutOfBoundsException\"
  indexa ask --top-k 20 \"Python files using async\"")]
    Ask {
        /// Natural-language question.
        question: String,

        /// Embedding model (must match what was used during indexing; overrides config).
        #[arg(long)]
        embed_model: Option<String>,

        /// Generation model for answer synthesis (overrides config).
        #[arg(long)]
        llm_model: Option<String>,

        /// Limit search to files under this path.
        #[arg(long)]
        scope: Option<String>,

        /// Number of chunks to retrieve before synthesis (overrides config).
        #[arg(long)]
        top_k: Option<usize>,

        /// Use keyword-only (BM25) search; no embedder call required.
        #[arg(long, conflicts_with_all = ["dense_only"])]
        sparse_only: bool,

        /// Use semantic (vector) search only; no FTS query.
        #[arg(long, conflicts_with_all = ["sparse_only"])]
        dense_only: bool,
    },

    /// Watch one or more paths for changes and keep their context current.
    #[command(after_help = "Examples:
  indexa watch ~/Documents
  indexa watch ~/Documents ~/Projects")]
    Watch {
        /// Paths to watch. Omit to watch the home directory.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Embedding model to use (overrides config).
        #[arg(long)]
        embed_model: Option<String>,
    },

    /// Start the local web UI at http://localhost:<port>.
    #[command(after_help = "Examples:
  indexa serve
  indexa serve --port 8080")]
    Serve {
        #[arg(short, long, default_value_t = 7620)]
        port: u16,

        /// Embedding model to use (overrides config).
        #[arg(long)]
        embed_model: Option<String>,

        /// Generation model to use (overrides config).
        #[arg(long)]
        llm_model: Option<String>,
    },

    /// Run the MCP (Model Context Protocol) server over stdio for AI agents.
    ///
    /// Exposes the index as agent tools (search, browse_tree, get_summary,
    /// read_file, ask, get_stats) so clients like Claude Desktop and Cursor can
    /// browse your local context live. Communicates over stdin/stdout.
    #[command(after_help = "Examples:
  indexa mcp
  # Claude Desktop config: { \"command\": \"indexa\", \"args\": [\"mcp\"] }")]
    Mcp {},

    /// Show context store statistics.
    #[command(after_help = "Examples:
  indexa status
  indexa status --unknown")]
    Status {
        /// Print the top-20 file extensions that could not be classified.
        #[arg(long)]
        unknown: bool,
    },

    /// Remove one or more paths from the context store.
    #[command(after_help = "Examples:
  indexa rm ~/Documents/old-project
  indexa rm -r ~/Documents/old-folder")]
    Rm {
        /// Paths to remove from the index.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,

        /// Also remove all entries under each path (for directories).
        #[arg(short, long)]
        recursive: bool,
    },

    /// Detect your machine's specs, recommend AI models, and estimate job times.
    ///
    /// Run this before your first deep/summarize job to understand what Indexa
    /// will do with your hardware and how long it will take.
    #[command(after_help = "Examples:
  indexa doctor
  indexa doctor --profile conservative
  indexa doctor --files 500 --chunks 2000")]
    Doctor {
        /// Resource profile to evaluate: conservative, balanced (default), performance.
        #[arg(long, default_value = "balanced")]
        profile: String,

        /// Estimated number of files for ETA calculation (overrides detection).
        #[arg(long)]
        files: Option<usize>,

        /// Estimated number of embedding chunks for ETA calculation.
        #[arg(long)]
        chunks: Option<usize>,
    },

    /// Detect software and project types in the index by file-pattern signatures.
    ///
    /// Reports things like Rust crates, Node/Next.js apps, Docker Compose stacks, and
    /// Helm charts found across your indexed folders — without reading file contents.
    /// Extend the catalog with a `fingerprints.json` next to your config (see docs).
    #[command(after_help = "Examples:
  indexa fingerprint
  indexa fingerprint --paths   # also list the matching directories")]
    Fingerprint {
        /// List the matching directory paths under each detected type.
        #[arg(long)]
        paths: bool,
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
                ..
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

    #[test]
    fn cli_ask_scope_and_sparse_only() {
        let cli = Cli::try_parse_from([
            "indexa",
            "ask",
            "find tax docs",
            "--scope",
            "~/Documents",
            "--sparse-only",
        ])
        .unwrap();
        match cli.command {
            Commands::Ask {
                scope, sparse_only, ..
            } => {
                assert_eq!(scope.as_deref(), Some("~/Documents"));
                assert!(sparse_only);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_sparse_and_dense_conflict() {
        assert!(
            Cli::try_parse_from(["indexa", "ask", "q", "--sparse-only", "--dense-only"]).is_err()
        );
    }

    #[test]
    fn cli_deep_dry_run() {
        let cli = Cli::try_parse_from(["indexa", "deep", "~/Documents", "--dry-run"]).unwrap();
        match cli.command {
            Commands::Deep { dry_run, .. } => assert!(dry_run),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_rm_recursive() {
        let cli = Cli::try_parse_from(["indexa", "rm", "-r", "~/old"]).unwrap();
        match cli.command {
            Commands::Rm { paths, recursive } => {
                assert!(recursive);
                assert_eq!(paths, vec!["~/old"]);
            }
            _ => panic!("wrong command"),
        }
    }
}
