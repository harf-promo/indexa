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
indexa index ~/code/myrepo             # build full context in one command\n  \
indexa ask \"where is auth handled?\"    # grounded answer with sources\n  \
indexa export ~/code/myrepo > ctx.xml  # export as XML for your AI tool\n  \
indexa serve                           # open local web UI\n\n\
(Power users: the pipeline stages behind `index` — scan, deep, summarize, worker, watch — \
are individually scriptable.)",
    after_help = "Command groups:\n  \
Core      index · ask · search · export · serve · status\n  \
Manage    pack · weight · classify · saved · rm · prune · review\n  \
Analyze   insights · graph · related · report · map · describe · fingerprint · snapshot · eval\n  \
Pipeline  scan · deep · summarize · worker · watch   (the stages behind `index`)\n  \
System    doctor · mcp · completion · update"
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
  indexa index ~/Projects --embed-model nomic-embed-text:v1.5
  indexa index ~/Projects --contextual   # Anthropic Contextual Retrieval (slower, better recall)")]
    #[command(display_order = 10)]
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

        /// Enable Anthropic Contextual Retrieval: generate a 1–2 sentence situating blurb
        /// per chunk before embedding. Reduces retrieval failures by ~35% at the cost of
        /// one extra LLM call per chunk (slower on local hardware). Default: off.
        /// Also enabled by `[describer] contextual_retrieval = true` in config.
        #[arg(long)]
        contextual: bool,
    },

    /// Build the surface context map of a path (fast — no AI calls).
    #[command(after_help = "Examples:
  indexa scan ~/Documents
  indexa scan ~/Projects ~/Notes
  indexa scan --all")]
    #[command(display_order = 40)]
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
    #[command(display_order = 34)]
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
  indexa deep --dry-run ~/Documents
  indexa deep ~/Projects --contextual   # Anthropic Contextual Retrieval (slower, better recall)")]
    #[command(display_order = 41)]
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

        /// Enable Anthropic Contextual Retrieval: generate a 1–2 sentence situating blurb
        /// per chunk before embedding. Reduces retrieval failures by ~35% at the cost of
        /// one extra LLM call per chunk (slower on local hardware). Default: off.
        /// Also enabled by `[describer] contextual_retrieval = true` in config.
        #[arg(long)]
        contextual: bool,
    },

    /// Generate hierarchical context summaries for indexed files and directories.
    #[command(after_help = "Examples:
  indexa summarize ~/Documents
  indexa summarize ~/Documents --mode compress
  indexa summarize ~/Documents --passes 2")]
    #[command(display_order = 42)]
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
    #[command(display_order = 35)]
    Describe {
        /// Path to describe.
        path: String,
    },

    /// Show exactly what's indexed for a path: entry, chunks, summary, classification, weight, graph.
    #[command(after_help = "Examples:
  indexa inspect ~/code/myrepo/src/main.rs
  indexa inspect ~/Documents/taxes")]
    #[command(display_order = 36)]
    Inspect {
        /// Path to inspect.
        path: String,
    },

    /// Run the background summarization worker (drains the summary queue).
    #[command(after_help = "Examples:
  indexa worker
  indexa worker --concurrency 4")]
    #[command(display_order = 43)]
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
    #[command(display_order = 20)]
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
    #[command(display_order = 21)]
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
    #[command(display_order = 30)]
    Insights {
        #[command(subcommand)]
        action: InsightsAction,
    },

    /// Save and re-run named queries (reusable `ask` searches).
    #[command(after_help = "Examples:
  indexa saved add priorities \"what are my current priorities?\"
  indexa saved run priorities
  indexa saved list")]
    #[command(display_order = 23)]
    Saved {
        #[command(subcommand)]
        action: SavedAction,
    },

    /// Review the questions Indexa asked — the Decision Ledger inbox.
    ///
    /// When indexing hits an uncertain judgment (a folder's category, a duplicate
    /// cluster's canonical copy) it opens a question here instead of guessing
    /// silently. Every answer is recorded with full history and can be reverted;
    /// nothing is ever silently overridden.
    #[command(after_help = "Examples:
  indexa review list
  indexa review show 12
  indexa review answer 12 work
  indexa review answer --type classification --under ~/Downloads --choose archive
  indexa review history ~/Downloads/reports
  indexa review revert 12
  indexa review scan")]
    #[command(display_order = 26)]
    Review {
        #[command(subcommand)]
        action: ReviewAction,
    },

    /// Export/import a portable snapshot of the index's summaries, call graph, and weights.
    #[command(after_help = "Examples:
  indexa snapshot export -o myindex.snapshot.json
  indexa snapshot import myindex.snapshot.json   # into a fresh index")]
    #[command(display_order = 37)]
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },

    /// Regression-test retrieval quality against a golden-questions file (no LLM).
    ///
    /// Runs each question through the same retrieval the `ask` pipeline uses and
    /// scores the ranked hits against the paths you expect: hit@k, MRR, and citation
    /// precision. Sparse mode (the default) needs no embedder or Ollama, so it can
    /// gate CI. Golden file format: docs/how-to/evaluate-retrieval.md.
    #[command(after_help = "Examples:
  indexa eval golden.json
  indexa eval golden.json --mode rrf --top-k 20
  indexa eval golden.json --json --min-hit-rate 0.8   # exit 1 below 80% hit rate")]
    #[command(display_order = 38)]
    Eval {
        /// Golden-questions JSON file (see docs/how-to/evaluate-retrieval.md).
        golden: String,

        /// Retrieval mode: sparse (default; no embedder needed), rrf, dense.
        #[arg(long, default_value = "sparse")]
        mode: String,

        /// Hits to retrieve per question when a question doesn't set its own `k`.
        #[arg(long, default_value_t = 10)]
        top_k: usize,

        /// Limit retrieval to files under this path.
        #[arg(long)]
        scope: Option<String>,

        /// Emit per-question metrics and the aggregate as JSON.
        #[arg(long)]
        json: bool,

        /// Exit 1 when the aggregate hit rate falls below this fraction (0.0–1.0).
        #[arg(long, default_value_t = 0.0)]
        min_hit_rate: f64,
    },

    /// Run several questions and render one document (answers + cited sources + TOC).
    #[command(after_help = "Examples:
  indexa report \"what is the architecture?\" \"how does auth work?\" > onboarding.md
  indexa report --saved priorities --saved risks --format xml -o report.xml")]
    #[command(display_order = 33)]
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
    #[command(display_order = 31)]
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
    #[command(display_order = 32)]
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
    #[command(display_order = 13)]
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

        /// Emit a code-skeleton view (symbol signatures, bodies elided) instead of prose
        /// summaries — feeds code structure to an AI tool at a fraction of the tokens. Reads
        /// indexed chunks, so it works after `deep` even without summaries.
        #[arg(long)]
        signatures: bool,

        /// Warn when the export exceeds this many estimated tokens (≈4 chars/token).
        #[arg(long)]
        token_budget: Option<usize>,

        /// With --token-budget, fail (non-zero exit) instead of warning when over budget — for CI.
        #[arg(long)]
        strict_budget: bool,

        /// Copy the export to the OS clipboard instead of writing a file / stdout.
        #[arg(long)]
        clipboard: bool,

        /// In --signatures mode, drop leading doc-comments from each signature.
        #[arg(long)]
        strip_comments: bool,

        /// Do NOT scan the export for secrets before output (redaction is on by default).
        #[arg(long)]
        no_redact: bool,
    },

    /// Query your local context with a natural-language question.
    #[command(after_help = "Examples:
  indexa ask \"where are my tax documents?\"
  indexa ask \"where is auth handled in this repo?\"
  indexa ask --scope ~/Work \"what are my current priorities?\"
  indexa ask --sparse-only \"IndexOutOfBoundsException\"
  indexa ask --top-k 20 \"Python files using async\"
  indexa ask --agentic \"how does indexing flow from scan to summary?\"  # multi-hop
  indexa ask --explain \"where is retrieval scored?\"   # show the retrieval trace
  indexa ask --json \"list the config files\" | jq -r '.sources[].path'")]
    #[command(display_order = 11)]
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
    #[command(display_order = 12)]
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
    #[command(display_order = 44)]
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
    #[command(display_order = 14)]
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
  indexa mcp                                  # run the stdio server
  indexa mcp install --client claude-code     # register with a client
  indexa mcp install --client cursor,vscode --dry-run
  # Claude Desktop config: { \"command\": \"indexa\", \"args\": [\"mcp\"] }")]
    #[command(display_order = 51)]
    Mcp {
        // Optional so bare `indexa mcp` keeps running the stdio server —
        // that invocation is what every client config points at.
        #[command(subcommand)]
        action: Option<McpAction>,
    },

    /// Show context store statistics.
    #[command(after_help = "Examples:
  indexa status
  indexa status --deep
  indexa status --unknown")]
    #[command(display_order = 15)]
    Status {
        /// Print the top-20 file extensions that could not be classified.
        #[arg(long)]
        unknown: bool,

        /// Append an index-health report: chunk/embedding/summary coverage,
        /// stale summaries, and per-root last-indexed times.
        #[arg(long)]
        deep: bool,

        /// Emit the status as a JSON object for scripting/CI.
        #[arg(long)]
        json: bool,
    },

    /// Remove one or more paths from the context store.
    #[command(after_help = "Examples:
  indexa rm ~/Documents/old-project
  indexa rm -r ~/Documents/old-folder")]
    #[command(display_order = 24)]
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
    #[command(display_order = 25)]
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
    #[command(display_order = 50)]
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

        /// Apply the recommended Ollama server env vars (KEEP_ALIVE=30s,
        /// MAX_LOADED_MODELS=1, NUM_PARALLEL=1). On macOS runs `launchctl setenv`
        /// (then quit + relaunch Ollama); elsewhere prints the `export` lines to add.
        #[arg(long)]
        apply_ollama_env: bool,
    },

    /// Detect software and project types in the index by file-pattern signatures.
    ///
    /// Reports things like Rust crates, Node/Next.js apps, Docker Compose stacks, and
    /// Helm charts found across your indexed folders — without reading file contents.
    /// Extend the catalog with a `fingerprints.json` next to your config (see docs).
    #[command(after_help = "Examples:
  indexa fingerprint
  indexa fingerprint --paths   # also list the matching directories")]
    #[command(display_order = 36)]
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
  indexa classify                       # suggest a category for every folder
  indexa classify --paths               # also print the folders under each category
  indexa classify --category code       # only the 'code' folders
  indexa classify --category work --paths
  # Confirm / correct / ignore suggestions in the web UI (Settings → Smart classification)
  # or over MCP (confirm_classification / ignore_classification / list_files_by_category).")]
    #[command(display_order = 22)]
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
    #[command(display_order = 52)]
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

    /// Print a shell-completion script for your shell.
    ///
    /// Pipe it into the right place for your shell, e.g.:
    ///   bash: indexa completion bash > /usr/local/etc/bash_completion.d/indexa
    ///   zsh:  indexa completion zsh  > "${fpath[1]}/_indexa"
    ///   fish: indexa completion fish > ~/.config/fish/completions/indexa.fish
    #[command(after_help = "Examples:
  indexa completion zsh > \"${fpath[1]}/_indexa\"
  indexa completion bash | sudo tee /etc/bash_completion.d/indexa
  indexa completion fish > ~/.config/fish/completions/indexa.fish")]
    #[command(display_order = 52)]
    Completion {
        /// Target shell: bash, zsh, fish, powershell, or elvish.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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
    /// Fetch a remote source (GitHub issue/PR or web page) into a pack as a cached Markdown file.
    /// Opt-in: set `[sources] enabled = true` or `INDEXA_REMOTE_FETCH_ALLOW=1` (reaches the network).
    #[command(
        name = "add-url",
        after_help = "Examples:
  INDEXA_REMOTE_FETCH_ALLOW=1 indexa pack add-url \"Bug 219\" https://github.com/harf-promo/indexa/issues/219
  indexa pack add-url \"Docs\" https://docs.example.com/guide --label guide"
    )]
    AddUrl {
        /// Pack name.
        name: String,
        /// URL to fetch (a GitHub issue/PR, or any web page).
        url: String,
        /// Optional label for the cached file name (defaults to a slug of the URL).
        #[arg(long)]
        label: Option<String>,
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
        /// Emit a code-skeleton view (symbol signatures, bodies elided) instead of summaries.
        #[arg(long)]
        signatures: bool,
        /// Warn when the export exceeds this many estimated tokens (≈4 chars/token).
        #[arg(long)]
        token_budget: Option<usize>,
        /// With --token-budget, fail instead of warning when over budget — for CI.
        #[arg(long)]
        strict_budget: bool,
        /// Copy the export to the OS clipboard instead of a file / stdout.
        #[arg(long)]
        clipboard: bool,
        /// In --signatures mode, drop leading doc-comments from each signature.
        #[arg(long)]
        strip_comments: bool,
        /// Do NOT scan the export for secrets before output (redaction is on by default).
        #[arg(long)]
        no_redact: bool,
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

/// Sub-commands for `indexa snapshot`.
/// Sub-commands for `indexa mcp`.
#[derive(clap::Subcommand, Debug)]
pub enum McpAction {
    /// Register Indexa as an MCP server in one or more AI clients.
    ///
    /// Writes only the `indexa` server entry into each client's config,
    /// leaving every other key untouched (a .bak is kept when the file existed).
    /// With no `--client`, auto-detects which supported clients are installed
    /// (config present / `claude` on PATH / a `.vscode` workspace) and configures
    /// each one found.
    #[command(after_help = "Examples:
  indexa mcp install                          # auto-detect installed clients
  indexa mcp install --client claude-code
  indexa mcp install --client claude-desktop --client cursor
  indexa mcp install --client cursor,vscode --dry-run")]
    Install {
        /// Client(s) to configure: claude-code, claude-desktop, cursor, vscode.
        /// Repeat the flag or pass a comma-separated list. Omit to auto-detect
        /// every supported client that appears to be installed.
        #[arg(long, value_delimiter = ',')]
        client: Vec<String>,

        /// Print the commands/JSON that would be written without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum SnapshotAction {
    /// Export a versioned snapshot (summaries + call graph + weights) as JSON.
    Export {
        /// Write to FILE instead of stdout.
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Import a snapshot into a fresh index (one with no summaries).
    Import {
        /// Snapshot JSON file to load.
        file: String,
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

/// Sub-commands for `indexa review`.
#[derive(clap::Subcommand, Debug)]
pub enum ReviewAction {
    /// List open questions, highest priority first.
    List {
        /// Only questions of this type: classification or duplicate.
        #[arg(long = "type", value_name = "TYPE")]
        decision_type: Option<String>,
    },
    /// Show one question in full: rendering, raw evidence, and revision chain.
    Show {
        /// Question id (from `review list`).
        id: i64,
    },
    /// Answer one question, or batch-answer with --type/--under/--choose.
    Answer {
        /// Question id (from `review list`).
        #[arg(
            required_unless_present = "under",
            conflicts_with_all = ["decision_type", "under", "choose"]
        )]
        id: Option<i64>,
        /// Option value to answer with, exactly as listed.
        #[arg(required_unless_present = "under", conflicts_with = "under")]
        choice: Option<String>,
        /// Batch: answer questions of this type (classification or duplicate).
        #[arg(long = "type", value_name = "TYPE", requires = "under")]
        decision_type: Option<String>,
        /// Batch: every open question whose subject is under this directory.
        #[arg(
            long,
            value_name = "DIR",
            requires = "decision_type",
            requires = "choose"
        )]
        under: Option<String>,
        /// Batch: the option value to answer each matched question with.
        #[arg(long, value_name = "VALUE", requires = "under")]
        choose: Option<String>,
    },
    /// Dismiss a question — it only returns if its evidence changes.
    Dismiss {
        /// Question id.
        id: i64,
    },
    /// Show every decision recorded about a path, oldest first.
    History {
        /// The path the decisions are about.
        path: String,
    },
    /// Restore an earlier answer by appending a new revision (never deletes).
    Revert {
        /// Id of the decided revision whose answer to restore.
        id: i64,
    },
    /// Run the detectors now (duplicate clusters + crash-repair sweep).
    Scan,
    /// Remove dismissed/expired questions older than the horizon.
    Gc {
        /// Age horizon in days.
        #[arg(long, default_value_t = 365)]
        older_than_days: i64,
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
    fn cli_deep_contextual_flag() {
        let cli = Cli::try_parse_from(["indexa", "deep", "~/Projects", "--contextual"]).unwrap();
        match cli.command {
            Commands::Deep { contextual, .. } => assert!(contextual),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_index_contextual_flag() {
        let cli = Cli::try_parse_from(["indexa", "index", "~/Projects", "--contextual"]).unwrap();
        match cli.command {
            Commands::Index { contextual, .. } => assert!(contextual),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn cli_review_answer_single() {
        let cli = Cli::try_parse_from(["indexa", "review", "answer", "12", "work"]).unwrap();
        match cli.command {
            Commands::Review {
                action: ReviewAction::Answer { id, choice, .. },
            } => {
                assert_eq!(id, Some(12));
                assert_eq!(choice.as_deref(), Some("work"));
            }
            _ => panic!("wrong command"),
        }
        // <choice> is mandatory alongside <id>.
        assert!(Cli::try_parse_from(["indexa", "review", "answer", "12"]).is_err());
    }

    #[test]
    fn cli_review_answer_batch_flags() {
        let cli = Cli::try_parse_from([
            "indexa",
            "review",
            "answer",
            "--type",
            "classification",
            "--under",
            "/tmp",
            "--choose",
            "archive",
        ])
        .unwrap();
        match cli.command {
            Commands::Review {
                action:
                    ReviewAction::Answer {
                        id,
                        decision_type,
                        under,
                        choose,
                        ..
                    },
            } => {
                assert!(id.is_none());
                assert_eq!(decision_type.as_deref(), Some("classification"));
                assert_eq!(under.as_deref(), Some("/tmp"));
                assert_eq!(choose.as_deref(), Some("archive"));
            }
            _ => panic!("wrong command"),
        }
        // --under without --type/--choose is incomplete.
        assert!(Cli::try_parse_from(["indexa", "review", "answer", "--under", "/tmp"]).is_err());
        // Positional <id> and the batch flags are mutually exclusive.
        assert!(Cli::try_parse_from([
            "indexa",
            "review",
            "answer",
            "12",
            "work",
            "--under",
            "/tmp",
            "--type",
            "classification",
            "--choose",
            "archive",
        ])
        .is_err());
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

    #[test]
    fn cli_parses_eval_with_defaults_and_threshold() {
        let cli = Cli::try_parse_from(["indexa", "eval", "golden.json", "--min-hit-rate", "0.8"])
            .unwrap();
        match cli.command {
            Commands::Eval {
                golden,
                mode,
                top_k,
                scope,
                json,
                min_hit_rate,
            } => {
                assert_eq!(golden, "golden.json");
                assert_eq!(mode, "sparse", "hermetic sparse is the default");
                assert_eq!(top_k, 10);
                assert!(scope.is_none());
                assert!(!json);
                assert!((min_hit_rate - 0.8).abs() < 1e-9);
            }
            _ => panic!("wrong command"),
        }
    }
}
