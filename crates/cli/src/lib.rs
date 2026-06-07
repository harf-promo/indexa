use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "indexa",
    version,
    arg_required_else_help = true,
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
    /// Build full context in one command: scan → deep embed → summarize.
    ///
    /// Equivalent to running `indexa scan`, `indexa deep`, and `indexa summarize`
    /// in sequence. Use this for first-time setup or complete refreshes.
    /// After it completes, run `indexa ask` or `indexa export`.
    #[command(after_help = "Examples:
  indexa index ~/code/my-repo
  indexa index ~/Documents --passes 2
  indexa index ~/Projects --embed-model nomic-embed-text:v1.5")]
    Index {
        /// Path(s) to index. Omit to index all existing roots.
        #[arg(num_args = 0..)]
        paths: Vec<String>,

        /// Embedding model to use (overrides config).
        #[arg(long)]
        embed_model: Option<String>,

        /// Summary storage mode: augment (default), compress, summaries-only.
        #[arg(long, default_value = "augment")]
        mode: String,

        /// Refinement passes per summary. Default: 2 for new context, 1 for refresh.
        #[arg(long)]
        passes: Option<u32>,
    },

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
        /// Before draining, re-index (scan→deep→summarize) any indexed root whose content
        /// is older than `[scan] auto_reindex` (default 7d if that's unset). Incremental.
        #[arg(long)]
        auto_reindex: bool,
    },

    /// Manage Context Packs — named, cross-directory context bundles.
    ///
    /// A Context Pack is a curated set of paths that form one topic ("Auth",
    /// "Tax 2025", "Client X"), even if they span different directories.
    /// Build a pack once, export it as XML/Markdown for any AI tool.
    #[command(after_help = "Examples:
  indexa pack create \"Auth\" --description \"Auth and session handling\"
  indexa pack add \"Auth\" ~/code/myrepo/src/auth
  indexa pack list
  indexa pack export \"Auth\" --format xml > auth-context.xml
  indexa pack show \"Auth\"
  indexa pack delete \"Auth\"")]
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },

    /// Manage importance weights — boost or suppress files, folders, or categories in search.
    #[command(after_help = "Examples:
  indexa weight set ~/Work/activeproject 2.0   # boost an active project
  indexa weight set ~/Archive 0.1              # suppress an archive folder
  indexa weight set --kind category code 1.5   # boost all 'code' classified dirs
  indexa weight get ~/Work/activeproject
  indexa weight list
  indexa weight suggest --days 30              # show recency-based suggestions
  indexa weight apply --days 7                 # auto-apply recency boosts")]
    Weight {
        #[command(subcommand)]
        action: WeightAction,
    },

    /// Analyse the index for duplicate files, stale projects, and recent changes.
    #[command(after_help = "Examples:
  indexa insights duplicates
  indexa insights duplicates --exact
  indexa insights stale --days 365
  indexa insights diff --days 7")]
    Insights {
        #[command(subcommand)]
        action: InsightsAction,
    },

    /// Save and re-run named queries (reusable `ask` searches).
    #[command(after_help = "Examples:
  indexa saved add priorities \"what are my current priorities?\"
  indexa saved run priorities
  indexa saved list")]
    Saved {
        #[command(subcommand)]
        action: SavedAction,
    },

    /// Run several questions and render one document (answers + cited sources + TOC).
    #[command(after_help = "Examples:
  indexa report \"what is the architecture?\" \"how does auth work?\" > onboarding.md
  indexa report --saved priorities --saved risks --format xml -o report.xml")]
    Report {
        /// Questions to answer (any number).
        #[arg(num_args = 0..)]
        questions: Vec<String>,
        /// Include a saved query by name (repeatable).
        #[arg(long)]
        saved: Vec<String>,
        /// Output format: md (default) or xml.
        #[arg(long, default_value = "md")]
        format: String,
        /// Write to FILE instead of stdout.
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Show the file-to-file call graph for a directory (who calls whom).
    #[command(after_help = "Examples:
  indexa graph ~/code/myrepo
  indexa graph ~/code/myrepo/src --limit 50")]
    Graph {
        /// Directory to scope the graph to.
        path: String,
        /// Max edges to print, heaviest first (default 100).
        #[arg(long, default_value = "100")]
        limit: usize,
        /// Strict resolution: only link calls to symbols defined in exactly one file
        /// (drops name-collision false positives). Default is the broader bare-name match.
        #[arg(long)]
        strict: bool,

        /// Report dependency cycles (strongly-connected components) instead of the graph.
        #[arg(long)]
        cycles: bool,
    },

    /// Find files related to a file via the call graph (it calls them, or they call it).
    #[command(after_help = "Examples:
  indexa related src/store/mod.rs
  indexa related --json --limit 5 crates/core/src/lib.rs")]
    Related {
        /// File to find relations for.
        path: String,
        /// Max related files to return.
        #[arg(long, default_value = "15")]
        limit: usize,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
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

        /// Append an importance-weights section (which files you've marked as important).
        #[arg(long)]
        include_weights: bool,

        /// Append the file-to-file call graph for the exported scope (heaviest edges).
        #[arg(long)]
        include_graph: bool,
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

        /// Agentic multi-hop retrieval: plan → search → refine across several hops
        /// before answering. Better on compositional questions; costs a few extra
        /// model calls. Off by default (overrides `[retrieval] agentic`).
        #[arg(long)]
        agentic: bool,

        /// Max retrieval hops in agentic mode (1..=5; overrides config). Implies --agentic.
        #[arg(long)]
        max_steps: Option<usize>,

        /// Print the retrieval pipeline (sparse, dense, and fused/reranked hits with
        /// scores) before the answer, to debug why specific sources were chosen.
        #[arg(long, conflicts_with_all = ["agentic", "max_steps"])]
        explain: bool,

        /// Emit the answer (and `--explain` trace, if set) as JSON for scripting.
        #[arg(long)]
        json: bool,
    },

    /// Search indexed content and print ranked hits — no LLM synthesis (that's `ask`).
    ///
    /// Defaults to fast keyword (BM25) search that needs no embedder, so it works even
    /// with Ollama down. Use --dense for semantic-only or --hybrid for both.
    #[command(after_help = "Examples:
  indexa search \"async runtime\"
  indexa search --hybrid --top-k 20 \"retry backoff\"
  indexa search --json \"TODO\" | jq -r '.[].path'")]
    Search {
        /// Search query.
        query: String,
        /// Number of hits to return (default 10).
        #[arg(long)]
        top_k: Option<usize>,
        /// Limit to files under this path.
        #[arg(long)]
        scope: Option<String>,
        /// Semantic (vector) search only — requires embeddings.
        #[arg(long, conflicts_with = "hybrid")]
        dense: bool,
        /// Hybrid BM25 + vector (RRF) search — requires embeddings.
        #[arg(long)]
        hybrid: bool,
        /// Emit hits as JSON for scripting.
        #[arg(long)]
        json: bool,
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
  indexa serve --port 8080
  indexa serve --host 0.0.0.0          # LAN access (⚠ exposes all indexed files on network)")]
    Serve {
        #[arg(short, long, default_value_t = 7620)]
        port: u16,

        /// Bind host address. Default 127.0.0.1 (localhost only).
        /// Use 0.0.0.0 to expose on all interfaces (LAN access).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

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

        /// Emit the status as a JSON object for scripting/CI.
        #[arg(long)]
        json: bool,
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

    /// Garbage-collect orphaned index rows (chunks/summaries left behind after a root
    /// was removed). Cleans dangling data the normal pipeline can't reach.
    #[command(after_help = "Examples:
  indexa prune --dry-run   # show what would be removed
  indexa prune             # remove orphaned chunks/summaries")]
    Prune {
        /// Show what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
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

    /// Suggest a semantic category (work/personal/archive/media/code/system) for
    /// each folder in the index — content-free, from surface hints (Tier 0).
    ///
    /// Suggestions are saved so you can confirm, correct, or ignore them in the
    /// web UI. Folders that need content to tell work from personal are left as
    /// "pending" until deeper inference lands.
    #[command(after_help = "Examples:
  indexa classify
  indexa classify --category code --paths")]
    Classify {
        /// Show the matching folder paths under each category.
        #[arg(long)]
        paths: bool,

        /// Only show folders classified as this category.
        #[arg(long)]
        category: Option<String>,
    },

    /// Update indexa to the latest release by replacing the running binary.
    ///
    /// Downloads the prebuilt binary for this platform from GitHub Releases and
    /// atomically replaces the current executable. The new binary is active on
    /// the next invocation.
    #[command(after_help = "Examples:
  indexa update              # check, confirm, then update
  indexa update --check      # report only (exit 1 = update available, 0 = current)
  indexa update -y           # update without interactive prompt
  indexa update --pin v0.12.1  # install a specific release")]
    Update {
        /// Only check and report — do not download or replace.
        /// Exits 1 if an update is available, 0 if already current.
        #[arg(long)]
        check: bool,

        /// Skip the interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Install a specific release tag instead of the latest, e.g. `v0.12.1`.
        #[arg(long)]
        pin: Option<String>,
    },
}

/// Sub-commands for `indexa pack`.
#[derive(clap::Subcommand, Debug)]
pub enum PackAction {
    /// Create a new Context Pack.
    #[command(after_help = "Examples:
  indexa pack create \"Auth\"
  indexa pack create \"Auth\" --auto
  indexa pack create \"Auth\" --auto --yes --limit 30")]
    Create {
        /// Pack name (must be unique).
        name: String,
        /// Optional short description.
        #[arg(long, short)]
        description: Option<String>,
        /// Auto-suggest paths by finding summaries semantically related to the
        /// pack name. Requires an indexed + summarised subtree with embeddings.
        /// Falls back to keyword search when embeddings are unavailable.
        #[arg(long, short)]
        auto: bool,
        /// Skip the confirmation prompt when using --auto.
        #[arg(long, short)]
        yes: bool,
        /// Number of paths to suggest when using --auto (default: 20).
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Add one or more paths to an existing pack.
    Add {
        /// Pack name.
        name: String,
        /// Paths to add (files or directories).
        #[arg(num_args = 1..)]
        paths: Vec<String>,
    },
    /// Remove one or more paths from a pack.
    Remove {
        /// Pack name.
        name: String,
        /// Paths to remove.
        #[arg(num_args = 1..)]
        paths: Vec<String>,
    },
    /// List all Context Packs.
    List,
    /// Show the paths inside a pack.
    Show {
        /// Pack name.
        name: String,
    },
    /// Export a pack as XML, Markdown, or JSON — ready to paste into any AI tool.
    #[command(after_help = "Examples:
  indexa pack export \"Auth\" --format xml > auth.xml
  indexa pack export \"Auth\" --format md
  indexa pack export \"Auth\" --format json --output auth.json")]
    Export {
        /// Pack name.
        name: String,
        /// Output format: xml (default), md, json.
        #[arg(long, default_value = "xml")]
        format: String,
        /// Write to a file instead of stdout.
        #[arg(long, short)]
        output: Option<String>,
        /// Maximum tree depth per path (0 = top summary only).
        #[arg(long)]
        depth: Option<usize>,
        /// Append an importance-weights section (which files you've marked as important).
        #[arg(long)]
        include_weights: bool,
    },
    /// Rename a pack.
    Rename {
        /// Current pack name.
        name: String,
        /// New pack name (must be unique).
        new_name: String,
    },
    /// Delete a pack (does not remove the indexed files — only the pack record).
    Delete {
        /// Pack name.
        name: String,
    },
}

/// Sub-commands for `indexa weight`.
#[derive(clap::Subcommand, Debug)]
pub enum WeightAction {
    /// Set an importance weight for a file, directory, or category.
    Set {
        /// Target path or category name.
        target: String,
        /// Weight value (0.0 = silence, 1.0 = neutral, >1.0 = boost).
        weight: f32,
        /// Target kind: file, dir, or category (default: auto-detect from path).
        #[arg(long, default_value = "auto")]
        kind: String,
    },
    /// Show the resolved weight for a path.
    Get {
        /// Absolute path to look up.
        path: String,
    },
    /// List all stored importance weights.
    List {
        /// Filter by kind: file, dir, or category.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Remove a stored weight.
    Delete {
        /// Target path or category name.
        target: String,
        /// Target kind: file, dir, or category.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Show auto-recency weight suggestions (does not apply them).
    Suggest {
        /// Consider files modified within this many days.
        #[arg(long, default_value = "30")]
        days: i64,
    },
    /// Apply auto-recency weights to the store.
    Apply {
        /// Consider files modified within this many days.
        #[arg(long, default_value = "30")]
        days: i64,
        /// Skip confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
}

/// Sub-commands for `indexa insights`.
#[derive(clap::Subcommand, Debug)]
pub enum InsightsAction {
    /// Find duplicate or near-duplicate files.
    Duplicates {
        /// Similarity threshold for near-duplicate detection (0.0–1.0).
        #[arg(long, default_value = "0.95")]
        threshold: f32,
        /// Find exact duplicates only (by content hash, no embedder required).
        #[arg(long)]
        exact: bool,
    },
    /// Find projects not modified for a long time.
    Stale {
        /// Report directories not modified in this many days.
        #[arg(long, default_value = "365")]
        days: i64,
    },
    /// Show what changed in the index over the past N days.
    Diff {
        /// Look back this many days.
        #[arg(long, default_value = "7")]
        days: i64,
    },
    /// List the largest indexed files (bloat detection).
    Largest {
        /// How many files to show.
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the language breakdown of indexed content (by chunk count).
    Languages {
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Sub-commands for `indexa saved`.
#[derive(clap::Subcommand, Debug)]
pub enum SavedAction {
    /// Save (or overwrite) a named query.
    Add {
        /// Name to save it under.
        name: String,
        /// The question.
        question: String,
        /// Retrieval mode: rrf (default) | sparse | dense | agentic.
        #[arg(long, default_value = "rrf")]
        mode: String,
        /// Limit retrieval to files under this path.
        #[arg(long)]
        scope: Option<String>,
    },
    /// List saved queries.
    List {
        /// Emit as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a saved query through the `ask` pipeline.
    Run {
        /// Saved query name.
        name: String,
        /// Emit the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a saved query.
    Rm {
        /// Saved query name.
        name: String,
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
