//! Configuration loader for `~/.indexa/config.toml`.
//!
//! All fields have sensible defaults — a missing or empty config file is valid.
//! Unknown keys are silently ignored (deny_unknown_fields is off) so older config
//! files stay compatible with newer binaries.

use crate::resource::ResourceProfile;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub embedding: EmbeddingConfig,
    pub chunking: ChunkingConfig,
    pub retrieval: RetrievalConfig,
    pub describer: DescriberConfig,
    pub parsers: ParsersConfig,
    /// Resource-awareness settings: memory headroom, model selection, ETA.
    pub resource: ResourceConfig,
    /// Per-directory overrides. Matched by path prefix (longest wins).
    #[serde(default)]
    pub region: Vec<RegionConfig>,
    /// Optional cloud-provider API keys persisted to config.toml.
    #[serde(default)]
    pub api_keys: ApiKeysConfig,
    /// Model-catalog settings (optional online refresh source).
    #[serde(default)]
    pub models: ModelsConfig,
    /// Scan-time ignore settings (`.gitignore` respect + extra patterns).
    #[serde(default)]
    pub scan: ScanConfig,
    /// Decision-Ledger review settings (v0.22): when uncertainty becomes a question.
    #[serde(default)]
    pub review: ReviewConfig,
    /// Opt-in remote-source ingestion (v0.32): pull a web page / GitHub issue|PR into a pack.
    #[serde(default)]
    pub sources: SourcesConfig,
}

/// Settings for opt-in remote-source ingestion (`indexa pack add-url`). Off by default — fetching
/// a URL reaches the network, so it must be explicitly enabled here or via `INDEXA_REMOTE_FETCH_ALLOW=1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    /// Allow `pack add-url` to fetch remote content. Also unlockable per-run with the
    /// `INDEXA_REMOTE_FETCH_ALLOW=1` environment variable.
    pub enabled: bool,
    /// HTTP timeout (seconds) for a remote fetch.
    pub timeout_secs: u64,
    /// Retry attempts on transient HTTP failures (429/5xx/timeouts).
    pub max_retries: u32,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: 30,
            max_retries: 2,
        }
    }
}

// ── Review (Decision Ledger) ──────────────────────────────────────────────────

/// Knobs for the Decision Ledger's question flow. Confident auto judgments stay
/// out of the ledger entirely (anti-bloat); the caps are question-fatigue
/// controls so a whole-disk pass can never flood the inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    /// Auto judgments with confidence below this are recorded as open questions
    /// instead of being silently applied (the band is bounded below by
    /// `decisions::detectors::UNCERTAINTY_FLOOR`).
    pub auto_record_below: f32,
    /// Detectors stop opening new questions once this many are already open.
    pub max_open: usize,
    /// Max questions a single scan/classify pass may open.
    pub max_new_per_scan: usize,
    /// Surface "which definition is authoritative?" questions for bare-name symbols
    /// defined in multiple files. OFF by default: on idiomatic codebases (Rust `new`,
    /// `default`, `parse`, `build`, …) these are near-unanswerable and flood the inbox.
    /// Opt in only with a real polyglot symbol-resolution need. (v0.39)
    pub symbol_ambiguity: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            auto_record_below: 0.8,
            max_open: 50,
            max_new_per_scan: 20,
            symbol_ambiguity: false,
        }
    }
}

// ── Scan settings ─────────────────────────────────────────────────────────────

/// Controls what the directory walker skips, on top of the built-in skips for build
/// artifacts (`node_modules`, `target`, `.venv`, …) and caches/VCS internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Honor the scan root's `.gitignore` (its patterns, anchored at the root). Default on.
    pub respect_gitignore: bool,
    /// Extra gitignore-style patterns to skip (e.g. `["build/", "*.log", "vendor/"]`).
    pub ignore: Vec<String>,
    /// Re-index interval for `indexa worker --auto-reindex`: `"off"` (default) or a duration
    /// like `"7d"` / `"30d"` / `"12h"`. When set, the worker re-runs scan→deep→summarize for
    /// any indexed root whose newest content is older than this. The `--auto-reindex` flag must
    /// still be passed to activate it (so an expensive rebuild never starts implicitly).
    pub auto_reindex: String,
    /// Descend into sensitive credential directories (`.ssh`, `.gnupg`, `.aws`, browser profiles,
    /// macOS Keychains, password managers). Defaults to `false` — these are never walked unless
    /// explicitly opted in via `[scan] include_sensitive = true` or `--include-sensitive`.
    pub include_sensitive: bool,
    /// Redact obvious secrets (API keys, tokens, PEM private-key blocks) from chunk text at
    /// index time. Defaults to `true`. A second protection layer on top of the sensitive-path
    /// deny list — covers secrets accidentally committed to otherwise-included source trees.
    pub redact_at_index: bool,
    /// Skip binary files (NUL-sniffed) from content parsing during `deep`. Defaults to `false`
    /// so ordinary repo scans stay metadata-only (fast); enable for whole-computer indexing so
    /// executables/images/DB blobs aren't opened and parsed. The entry is still recorded either
    /// way — this only stops the deep phase from parsing flagged binaries.
    pub skip_binary: bool,
    /// Walker worker-thread count. `None` (default) = `available_parallelism()` floored at 4 — the
    /// walk is I/O/syscall-bound, so this scales past the core count. Set a number to cap on a
    /// shared host (leave cores for other tenants) or raise it on a fast local NVMe machine.
    pub threads: Option<usize>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            ignore: Vec::new(),
            auto_reindex: "off".to_owned(),
            include_sensitive: false,
            redact_at_index: true,
            skip_binary: false,
            threads: None,
        }
    }
}

/// Parse an auto-reindex interval string (`"7d"`, `"30d"`, `"12h"`, `"90m"`, `"3600s"`) to
/// seconds. `"off"`, empty, or `"0"` → `None` (disabled). An unrecognized unit/number → `None`.
pub fn parse_reindex_interval(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("off") || s == "0" {
        return None;
    }
    // Split off the last *char*, not the last byte: `split_at(len-1)` panics when the
    // string ends in a multibyte char (e.g. "7°"), and `s` can be user-controlled
    // (config TOML, `--changed-since`, the web `?changed_since=` query param) — this
    // path must fail open / return a clean error, never panic.
    let unit = s.chars().last()?;
    let num = &s[..s.len() - unit.len_utf8()];
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    n.checked_mul(mult).filter(|&v| v > 0)
}

// ── Model catalog ───────────────────────────────────────────────────────────

/// Settings for the local-model catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Optional URL to a JSON array of catalog entries used by
    /// `POST /api/models/catalog/refresh`. When unset, the refresh endpoint is a
    /// no-op and the bundled curated catalog is served. The fetch fails open:
    /// any error leaves the bundled/prior catalog in place.
    pub catalog_url: Option<String>,
}

// ── API keys ──────────────────────────────────────────────────────────────────

/// Optional cloud-provider API keys stored in config.toml.
/// These are used as fallback when the corresponding environment variables
/// (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GOOGLE_API_KEY`) are not set.
/// Keys are stored at rest — ensure config.toml has 0600 permissions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiKeysConfig {
    pub openai: Option<String>,
    pub anthropic: Option<String>,
    pub google: Option<String>,
}

// ── Embedding ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Provider: "ollama" | "openai" | "anthropic" | "llamacpp"
    pub provider: String,
    /// Model name (provider-specific).
    pub model: String,
    /// Embedding dimension. Must match the model's output.
    pub dim: usize,
    /// Base URL for the provider's API.
    pub base_url: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: "ollama".into(),
            model: "nomic-embed-text".into(),
            dim: 768,
            base_url: "http://localhost:11434".into(),
        }
    }
}

// ── Chunking ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChunkStrategy {
    /// Respect document structure (headings, AST nodes). Falls back to fixed.
    #[default]
    Structure,
    /// Fixed-size windows with overlap.
    Fixed,
    /// Split on sentence/paragraph boundaries (future).
    Recursive,
    /// Embed full doc, window embeddings (future — late chunking).
    Semantic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkingConfig {
    pub strategy: ChunkStrategy,
    /// Target words per chunk (approximate for structure mode).
    pub size: usize,
    /// Words of overlap between consecutive fixed-size chunks.
    pub overlap: usize,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            strategy: ChunkStrategy::Structure,
            size: 800,
            overlap: 100,
        }
    }
}

// ── Retrieval ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HybridMode {
    /// Reciprocal Rank Fusion (default).
    #[default]
    Rrf,
    /// Sparse results only (BM25/FTS5).
    Sparse,
    /// Dense results only (cosine similarity).
    Dense,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
    pub hybrid: HybridMode,
    /// RRF rank constant k (60 is the standard default).
    pub rrf_k: usize,
    /// Number of results to retrieve before reranking.
    pub top_k: usize,
    /// Rerank retrieved hits before synthesis (default on). With the default
    /// `"llm"` backend this reuses the already-loaded generation model — no
    /// extra dependency or download — and fails open. See `rerank_backend` for
    /// the higher-quality (opt-in) cross-encoder.
    pub rerank: bool,
    /// Weight of summary hits relative to chunk hits in RRF fusion (0.0 = disabled).
    pub summary_weight: f32,
    /// Depth-boost coefficient α: parent summaries score 1 + α*(max_depth - depth) higher.
    pub summary_depth_alpha: f32,
    /// Max characters of retrieved context packed into the answer-synthesis prompt.
    pub context_budget: usize,
    /// Use an in-memory HNSW (ANN) index for dense retrieval instead of a brute-force cosine
    /// scan. **On by default** — but only takes effect in a long-lived process (the web server
    /// and the MCP server, which cache the index) and above `ann_min_chunks`; below that, and for
    /// scoped queries, dense retrieval falls back to the exact brute-force scan. A one-shot CLI
    /// `ask` never builds the index (it would pay the full build cost for a single query). ANN is
    /// approximate but recall is high; set to `false` to force exact brute-force everywhere.
    pub ann: bool,
    /// Minimum chunk count before the ANN index is built/used (below this, brute-force is
    /// faster than building an index). Only consulted when `ann` is true.
    pub ann_min_chunks: usize,
    /// Apply importance weights (v0.8) as a multiplicative boost after RRF fusion.
    /// Weights are stored per-file/dir/category in the `importance_weights` table.
    pub use_weights: bool,
    /// Default the agentic multi-hop `ask` on (opt-in; `--agentic` / MCP `agentic`
    /// override per call). Off by default — agentic does a few extra LLM calls.
    pub agentic: bool,
    /// Max retrieval hops when agentic `ask` is used (clamped to 1..=5).
    pub agentic_max_steps: usize,
    /// Boost recently-modified files in retrieval (v0.31) — the positive twin of the archive
    /// penalty (fresh work outranks stale at comparable relevance). Off by default so it never
    /// silently re-ranks; uses filesystem mtime, not git.
    pub recency_boost: bool,
    /// Recency window in days for `recency_boost` (files older than this stay neutral).
    pub recency_days: i64,
    /// MMR (Maximal Marginal Relevance) lambda for retrieval diversity (v0.42).
    ///
    /// Controls the trade-off between relevance and diversity when re-ranking
    /// retrieved chunks before synthesis:
    /// - `1.0` — pure relevance (MMR disabled; identical to no-MMR behaviour).
    /// - `0.5` — balanced (default): similar to the top hit are demoted,
    ///   promoting diverse coverage of the question.
    /// - `0.0` — maximum diversity: every chunk selected maximises distance from
    ///   previously selected ones (ignores relevance entirely).
    ///
    /// MMR only runs when embeddings are available (dense or RRF mode) and
    /// `mmr_lambda < 1.0`; it fails open — any error fetching embeddings leaves
    /// the original rank order unchanged.
    ///
    /// TOML: `[retrieval] mmr_lambda = 0.3`
    pub mmr_lambda: f32,
    /// Backend used for cross-encoder reranking when `rerank = true` (v0.43).
    ///
    /// - `"llm"` (default): listwise rerank via the local generation model (no extra dep).
    /// - `"cross-encoder"`: pointwise rerank via a local DeBERTa-v2 model downloaded from
    ///   HuggingFace on first use and cached in `~/.cache/huggingface/hub/`. Model:
    ///   `mixedbread-ai/mxbai-rerank-xsmall-v1` (~85 MB, Apache-2.0, CPU-only).
    ///   Falls back to `"llm"` if the model can't be loaded.
    ///
    /// TOML: `[retrieval] rerank_backend = "cross-encoder"`
    pub rerank_backend: String,
    /// HuggingFace model for the `"cross-encoder"` rerank backend (v0.77). All three
    /// options share the same DeBERTa-v2 architecture, so they are drop-in — larger =
    /// higher quality, larger download:
    /// - `mixedbread-ai/mxbai-rerank-xsmall-v1` (default, ~85 MB) — the fast baseline.
    /// - `mixedbread-ai/mxbai-rerank-base-v1` (~370 MB) — stronger.
    /// - `mixedbread-ai/mxbai-rerank-large-v1` (~870 MB) — strongest.
    ///
    /// Ignored unless `rerank_backend = "cross-encoder"`. Loading failure falls open to
    /// `"llm"`. (The mxbai-rerank-**v2** family is Qwen-decoder-based and does NOT load
    /// through the DeBERTa path — not supported here.)
    ///
    /// TOML: `[retrieval] rerank_model = "mixedbread-ai/mxbai-rerank-base-v1"`
    pub rerank_model: String,
    /// Path segments that mark content as historical/superseded (matched case-insensitively
    /// and segment-bounded). Hits under such a path are down-weighted by `archive_penalty`
    /// (v0.29). Extend it (`legacy`/`attic`/`backup`) to suit your tree, or set it to an empty
    /// list to disable the archive penalty entirely.
    pub archive_segments: Vec<String>,
    /// Multiplicative penalty applied to hits whose path contains an `archive_segments`
    /// segment (v0.29). The default `0.15` keeps such hits retrievable (and explicitly
    /// scopeable) while letting current docs outrank them; `0.0` disables the penalty.
    pub archive_penalty: f64,
    /// GraphRAG-lite (v0.69): on a **broad, unscoped** question, the max chunks one file may
    /// contribute to the retrieved pool before other files get a turn — so one chunk-dense file
    /// can't monopolise the answer's context, giving thematic questions balanced multi-file
    /// coverage. `0` (default) disables it. Only applied when the question reads as broad/thematic
    /// AND no `scope` is set; focused and scoped `ask`s are unaffected. A small value (2–3) is
    /// typical. The reorder never drops a hit — it just defers a file's overflow chunks.
    pub broad_per_file_cap: usize,
    /// GraphRAG "Approach C" (v0.70): on a **broad, unscoped** question, group the retrieved hits
    /// into semantic clusters and hand the synthesizer topic-grouped context (with `graphrag_summarize`,
    /// a one-line theme per cluster) for a more coherent multi-faceted answer. `false` (default) ⇒
    /// today's flat packing, byte-identical. Only applied when the question reads as broad/thematic
    /// AND no `scope` is set. Restructures only the synthesis context — retrieval ranking is untouched.
    #[serde(default)]
    pub graphrag_clusters: bool,
    /// Max clusters when `graphrag_clusters` is on (also caps the per-cluster summarization calls).
    #[serde(default = "default_graphrag_max_clusters")]
    pub graphrag_max_clusters: usize,
    /// Cosine-similarity threshold for joining a hit to an existing cluster (`graphrag_clusters`).
    #[serde(default = "default_graphrag_cluster_sim")]
    pub graphrag_cluster_sim: f32,
    /// When `graphrag_clusters` is on, also summarize each multi-member cluster into a one-line theme
    /// with one extra local LLM call (≤ `graphrag_max_clusters` calls; fail-open). Separate sub-flag
    /// so clustering can be used without the added latency. `false` by default.
    #[serde(default)]
    pub graphrag_summarize: bool,
}

/// Default cluster cap for [`RetrievalConfig::graphrag_max_clusters`].
fn default_graphrag_max_clusters() -> usize {
    4
}

/// Default cosine threshold for [`RetrievalConfig::graphrag_cluster_sim`].
fn default_graphrag_cluster_sim() -> f32 {
    0.55
}

/// Default historical/superseded path segments (the v0.29 built-in set). Matched
/// case-insensitively and segment-bounded; drives [`RetrievalConfig::archive_segments`].
pub fn default_archive_segments() -> Vec<String> {
    ["archive", "archived", "historical", "deprecated", "old"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

/// Default archive down-weighting factor (the v0.29 built-in). Drives
/// [`RetrievalConfig::archive_penalty`].
pub const DEFAULT_ARCHIVE_PENALTY: f64 = 0.15;

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            hybrid: HybridMode::Rrf,
            rrf_k: 60,
            top_k: 12,
            rerank: true,
            summary_weight: 0.0,
            summary_depth_alpha: 0.15,
            context_budget: 8000,
            ann: true,
            ann_min_chunks: 50_000,
            use_weights: true,
            agentic: false,
            agentic_max_steps: 3,
            recency_boost: false,
            recency_days: 90,
            mmr_lambda: 0.5,
            rerank_backend: "llm".to_string(),
            rerank_model: "mixedbread-ai/mxbai-rerank-xsmall-v1".to_string(),
            archive_segments: default_archive_segments(),
            archive_penalty: DEFAULT_ARCHIVE_PENALTY,
            broad_per_file_cap: 0,
            graphrag_clusters: false,
            graphrag_max_clusters: default_graphrag_max_clusters(),
            graphrag_cluster_sim: default_graphrag_cluster_sim(),
            graphrag_summarize: false,
        }
    }
}

// ── Describer (answer synthesis LLM) ─────────────────────────────────────────

/// Whether to keep full chunks alongside summaries, replace them, or skip chunking.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryMode {
    /// Keep existing chunks + add summaries. Default: best answer quality.
    #[default]
    Augment,
    /// Summarize then drop chunk rows — ~10× smaller DB.
    Compress,
    /// Skip chunking entirely; file summaries only — ~100× smaller DB.
    SummariesOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DescriberConfig {
    pub provider: String,
    /// Model for Q&A answer synthesis.
    pub model: String,
    pub base_url: String,
    /// Enable Anthropic-style per-chunk contextual retrieval at index time — an LLM writes a
    /// situating blurb prepended to each chunk before embedding. Higher quality, but ~2–3× slower
    /// deep (one LLM call per chunk). See `contextual_prefix` for the free, local alternative.
    pub contextual_retrieval: bool,
    /// Enable the DETERMINISTIC contextual prefix at index time — prepend the file path, section
    /// heading, and a document-context snippet to each chunk's embed input (no LLM call, local and
    /// free). The local sibling of `contextual_retrieval`; if both are set, the LLM path wins.
    pub contextual_prefix: bool,
    /// Model for per-file summarization (smaller/faster is fine).
    pub file_model: String,
    /// Model for directory roll-up summaries (stronger model recommended).
    pub dir_model: String,
    /// Context window sent to Ollama as `num_ctx` for every summarization/Q&A call.
    /// Defaults to 4096 so the KV-cache matches what the resource budget assumes —
    /// omitting it lets Ollama load the model at its 32,768-token default and balloon
    /// the KV-cache ~8× past the budgeted footprint, driving swap blowout and freezes.
    pub num_ctx: u32,
    /// Storage mode for summaries.
    pub mode: SummaryMode,
    /// Concurrent summary worker tasks.
    pub queue_concurrency: usize,
    /// Max child summaries fed into a single directory roll-up prompt.
    pub max_children_per_summary: usize,
    /// Refinement passes when no prior summary exists (first-time build).
    pub passes_first: u32,
    /// Refinement passes when a summary row already exists (refresh).
    pub passes_refresh: u32,
    /// Hard ceiling on `--passes` flag; values above this are clamped.
    pub passes_cap: u32,
    /// Path to the `claude` CLI, used when `provider = "claude-code"` (runs
    /// summaries/answers on the user's Claude subscription instead of the metered
    /// API). Empty → resolved as "claude" on PATH.
    pub claude_bin: String,
    /// RUNTIME flag, not a config option (never read from / written to TOML):
    /// set by callers that auto-downgraded `file_model`/`dir_model` to fit the
    /// memory budget, so summary provenance records that a lighter model was
    /// substituted for the configured one.
    #[serde(skip)]
    pub model_fallback: bool,
}

impl Default for DescriberConfig {
    fn default() -> Self {
        Self {
            provider: "ollama".into(),
            model: "gemma3:12b".into(),
            base_url: "http://localhost:11434".into(),
            contextual_retrieval: false,
            contextual_prefix: false,
            file_model: "gemma3:4b".into(),
            dir_model: "gemma3:12b".into(),
            num_ctx: 4096,
            mode: SummaryMode::Augment,
            queue_concurrency: 2,
            max_children_per_summary: 30,
            passes_first: 2,
            passes_refresh: 1,
            passes_cap: 3,
            claude_bin: "claude".into(),
            model_fallback: false,
        }
    }
}

// ── Parser overrides ──────────────────────────────────────────────────────────

fn default_max_file_mb() -> u64 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ParsersConfig {
    pub pdf: PdfParserConfig,
    pub image: ImageParserConfig,
    pub audio: AudioParserConfig,
    pub video: VideoParserConfig,
    /// Maximum file size (MB) to attempt content parsing. Larger files are skipped to
    /// avoid reading huge files (logs, misclassified binaries) fully into memory.
    /// `0` disables the cap.
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
}

impl Default for ParsersConfig {
    fn default() -> Self {
        Self {
            pdf: PdfParserConfig::default(),
            image: ImageParserConfig::default(),
            audio: AudioParserConfig::default(),
            video: VideoParserConfig::default(),
            max_file_mb: default_max_file_mb(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfParserConfig {
    /// `"text"` (default) — text-layer extraction only (pdf-extract). `"ocr"` — additionally
    /// OCR pages with no text layer (scanned PDFs): rasterise with `pdftoppm` (poppler) and
    /// recognise with `tesseract`. Both are external tools; OCR is opt-in and fails open to
    /// the text layer when they're unavailable.
    pub backend: String,
    /// OCR engine binary when `backend = "ocr"` (default `tesseract`).
    pub ocr_binary: Option<String>,
    /// Optional tesseract language hint passed as `-l`, e.g. `"eng"` or `"eng+ara"`.
    pub ocr_lang: Option<String>,
}

impl Default for PdfParserConfig {
    fn default() -> Self {
        Self {
            backend: "text".into(),
            ocr_binary: None,
            ocr_lang: None,
        }
    }
}

impl PdfParserConfig {
    /// True when OCR of scanned (text-layer-less) PDFs is enabled.
    pub fn ocr_enabled(&self) -> bool {
        self.backend.eq_ignore_ascii_case("ocr")
    }
    /// OCR engine binary (defaults to `tesseract`).
    pub fn ocr_binary(&self) -> &str {
        self.ocr_binary.as_deref().unwrap_or("tesseract")
    }
}

/// Default Ollama vision model for image captioning. Reuses **gemma3** — the same Google
/// multimodal model Indexa already pulls for file summaries (per the documented setup) — so
/// captioning works out of the box with no extra model download, and the watchdog's existing
/// summary-model budget already covers it. Google-vendor per the project's model-preference
/// guidance. The user still opts in (`caption = true`); nothing is auto-downloaded. For
/// richer captions, set `[parsers.image] model = "gemma3:12b"` (also already pulled).
pub const DEFAULT_CAPTION_MODEL: &str = "gemma3:4b";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageParserConfig {
    /// Set true to caption images with an Ollama vision model (opt-in). Defaults to the gemma3
    /// summary model, which is already loaded — so there's no separate ~7-8 GB vision model to
    /// budget — but enable only with memory headroom. Images are sent to a local Ollama;
    /// nothing leaves the machine. Captions are produced on the next `deep` for newly-scanned
    /// or modified images; images already indexed (unchanged mtime) are skipped, so to caption
    /// an existing tree, touch the files or rebuild the index.
    pub caption: bool,
    /// Vision model to caption with. Defaults to [`DEFAULT_CAPTION_MODEL`] when unset.
    pub model: Option<String>,
}

impl ImageParserConfig {
    /// The vision model to use for captioning (configured value or the default).
    pub fn caption_model(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_CAPTION_MODEL)
    }
}

/// Default whisper.cpp-style CLI used for audio transcription. Users install it (and a
/// model); it is NOT bundled or auto-downloaded.
pub const DEFAULT_TRANSCRIBE_BINARY: &str = "whisper-cli";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioParserConfig {
    /// Set true to transcribe audio files by shelling out to a whisper.cpp-style CLI
    /// (opt-in). Requires the `binary` (default `whisper-cli`) on PATH and a `model`. The
    /// transcript is stored as a searchable chunk; the binary must accept the input format
    /// (whisper.cpp expects 16 kHz WAV — convert beforehand if needed). Only `audio/*` files
    /// are transcribed — extract the audio track from video files first. Like captioning,
    /// this applies to newly-scanned or modified files on the next `deep`.
    pub transcribe: bool,
    /// Transcription CLI to invoke. Defaults to [`DEFAULT_TRANSCRIBE_BINARY`].
    pub binary: Option<String>,
    /// Path to the whisper model file (passed as `-m`). Omitted when unset (the CLI then
    /// uses its own default model lookup).
    pub model: Option<String>,
}

impl AudioParserConfig {
    /// The transcription binary to invoke (configured value or the default).
    pub fn transcribe_binary(&self) -> &str {
        self.binary.as_deref().unwrap_or(DEFAULT_TRANSCRIBE_BINARY)
    }
}

/// Default ffmpeg binary for video frame extraction.
pub const DEFAULT_FFMPEG_BINARY: &str = "ffmpeg";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoParserConfig {
    /// Set true to caption video files by extracting frames and running them through
    /// a local vision model (opt-in). Requires `ffmpeg` on PATH for frame extraction
    /// and an Ollama vision model. Frames per second to sample is controlled by `fps_sample`.
    pub caption: bool,
    /// Ollama vision model. Defaults to [`DEFAULT_CAPTION_MODEL`].
    pub model: Option<String>,
    /// ffmpeg binary. Defaults to [`DEFAULT_FFMPEG_BINARY`].
    pub binary: Option<String>,
    /// Frames per second to sample from the video (default 0.5 = one frame every 2 s).
    pub fps_sample: Option<f32>,
    /// Maximum frames to caption per video (default 8 — caps LLM cost).
    pub max_frames: Option<usize>,
}

impl VideoParserConfig {
    /// The ffmpeg binary to use (configured value or the default).
    pub fn ffmpeg_binary(&self) -> &str {
        self.binary.as_deref().unwrap_or(DEFAULT_FFMPEG_BINARY)
    }
    /// Vision model (configured value or the image captioning default).
    pub fn caption_model(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_CAPTION_MODEL)
    }
    /// Frames-per-second to sample (configured or default 0.5).
    pub fn fps(&self) -> f32 {
        self.fps_sample.unwrap_or(0.5)
    }
    /// Max frames per video (configured or default 8).
    pub fn max_frames(&self) -> usize {
        self.max_frames.unwrap_or(8)
    }
}

// ── Per-region overrides ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionConfig {
    /// Directory path (supports ~ expansion).
    pub path: String,
    /// Optional parser overrides for this region.
    #[serde(default)]
    pub parsers: Option<ParsersConfig>,
    /// Optional embedding override for this region.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
}

// ── Resource configuration ────────────────────────────────────────────────────

/// Controls how aggressively Indexa uses system resources.
///
/// Indexa reads machine RAM and available memory before each AI job and
/// enforces a budget so the machine never freezes.  The `profile` is the
/// easiest knob; the individual fields let you fine-tune.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    /// High-level resource profile.  Drives headroom, keep_alive, and model
    /// selection defaults unless the individual fields are explicitly set.
    pub profile: ResourceProfile,

    /// Minimum RAM to keep free (GB).  Overrides the profile default when > 0.
    /// 0.0 = use the profile's built-in headroom.
    pub headroom_gb: f32,

    /// Automatically downgrade to a smaller model if the preferred one won't
    /// fit within the memory budget.  Default: true.
    pub auto_select_model: bool,

    /// Seconds to keep a model resident in Ollama after each call.
    /// 0 = unload immediately (most conservative).
    /// Overrides the profile default when > 0.
    pub keep_alive_secs: i64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            profile: ResourceProfile::Balanced,
            headroom_gb: 0.0, // 0 = use profile default
            auto_select_model: true,
            keep_alive_secs: 0, // 0 = use profile default
        }
    }
}

impl ResourceConfig {
    /// Effective headroom in bytes (explicit headroom_gb takes precedence over profile).
    pub fn effective_headroom_bytes(&self) -> u64 {
        if self.headroom_gb > 0.0 {
            (self.headroom_gb * 1024.0 * 1024.0 * 1024.0) as u64
        } else {
            self.profile.headroom_bytes()
        }
    }

    /// Effective keep_alive in seconds (explicit value takes precedence over profile).
    pub fn effective_keep_alive_secs(&self) -> i64 {
        if self.keep_alive_secs > 0 {
            self.keep_alive_secs
        } else {
            self.profile.keep_alive_secs()
        }
    }
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// Returns the canonical path to `~/.indexa/config.toml`
/// (or the platform-equivalent via `directories`).
/// Canonical bundle-ID used for config and data directories.
const APP_QUALIFIER: &str = "dev";
const APP_ORG: &str = "indexa";
const APP_NAME: &str = "Indexa";

pub fn default_config_path() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME) {
        dirs.config_dir().join("config.toml")
    } else {
        // Fallback: XDG-style ~/.indexa/
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".indexa").join("config.toml")
    }
}

/// Canonical data directory for the index database.
pub fn default_data_dir() -> Option<PathBuf> {
    ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME).map(|d| d.data_local_dir().to_path_buf())
}

/// Load config from `path`, returning `Config::default()` if the file is absent.
/// Returns an error only for parse failures, not for missing files.
pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }

    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config: {}", path.display()))?;

    toml::from_str(&text).with_context(|| format!("parsing config: {}", path.display()))
}

/// Load config from the default platform path.
pub fn load_default() -> Result<Config> {
    load(&default_config_path())
}

/// Serialise `cfg` to `path`, creating parent directories as needed.
/// On Unix, the file is written with `0600` permissions (owner read/write only)
/// to protect any stored API keys.
pub fn save(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir: {}", parent.display()))?;
    }
    let text =
        toml::to_string_pretty(cfg).with_context(|| "serialising config to TOML".to_owned())?;

    #[cfg(unix)]
    {
        // Create the (API-key-bearing) file with 0600 from the start so it is never briefly
        // world/group-readable between write and chmod — the old write-then-set_permissions
        // had a TOCTOU window. `mode()` only applies on creation, so also tighten an existing
        // file's perms afterward.
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing config: {}", path.display()))?;
        f.write_all(text.as_bytes())
            .with_context(|| format!("writing config: {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, &text)
            .with_context(|| format!("writing config: {}", path.display()))?;
    }
    Ok(())
}

// ── Region matching ───────────────────────────────────────────────────────────

impl Config {
    /// Find the region config whose path is the longest prefix of `target`.
    /// Performs ~ expansion on region paths before comparing.
    pub fn region_for(&self, target: &Path) -> Option<&RegionConfig> {
        self.region
            .iter()
            .filter_map(|r| {
                let expanded = shellexpand::tilde(&r.path);
                let region_path = Path::new(expanded.as_ref()).to_path_buf();
                if target.starts_with(&region_path) {
                    Some((region_path.components().count(), r))
                } else {
                    None
                }
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, r)| r)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reindex_interval_units_and_off() {
        assert_eq!(parse_reindex_interval("off"), None);
        assert_eq!(parse_reindex_interval(""), None);
        assert_eq!(parse_reindex_interval("0"), None);
        assert_eq!(parse_reindex_interval("OFF"), None);
        assert_eq!(parse_reindex_interval("7d"), Some(7 * 86_400));
        assert_eq!(parse_reindex_interval("30d"), Some(30 * 86_400));
        assert_eq!(parse_reindex_interval("12h"), Some(12 * 3600));
        assert_eq!(parse_reindex_interval("90m"), Some(90 * 60));
        assert_eq!(parse_reindex_interval("3600s"), Some(3600));
        // Garbage / unknown unit → None (treated as disabled, never a panic).
        assert_eq!(parse_reindex_interval("7w"), None);
        assert_eq!(parse_reindex_interval("abc"), None);
        assert_eq!(parse_reindex_interval("d"), None);
        // A multibyte trailing char must NOT panic (the old split_at(len-1) sliced a
        // non-char-boundary byte); it returns None like any other invalid unit.
        assert_eq!(parse_reindex_interval("7°"), None);
        assert_eq!(parse_reindex_interval("10日"), None);
        assert_eq!(parse_reindex_interval("°"), None);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_config_at_0600_and_tightens_existing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("indexa-cfg-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        // Fresh write must be 0600 — plaintext API keys live in this file.
        save(&Config::default(), &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh config must be 0600, got {mode:o}");

        // An existing world/group-readable file must be re-tightened to 0600 (the TOCTOU fix).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        save(&Config::default(), &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "existing config must re-tighten to 0600, got {mode:o}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_auto_reindex_roundtrips_and_defaults_off() {
        // Default is "off".
        assert_eq!(Config::default().scan.auto_reindex, "off");
        // Explicit value round-trips through TOML; other [scan] fields keep their defaults.
        let toml = r#"
[scan]
auto_reindex = "7d"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.scan.auto_reindex, "7d");
        assert!(cfg.scan.respect_gitignore);
    }

    #[test]
    fn default_config_is_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.embedding.model, "nomic-embed-text");
        assert_eq!(cfg.embedding.dim, 768);
        assert_eq!(cfg.retrieval.rrf_k, 60);
        // v0.44: wider retrieval + rerank-on by default (LLM backend, no download, fails open).
        assert_eq!(cfg.retrieval.top_k, 12);
        assert_eq!(cfg.retrieval.context_budget, 8000);
        assert!(cfg.retrieval.rerank);
        // ANN on by default: fast HNSW dense retrieval in long-lived processes above
        // ann_min_chunks; brute-force fallback below it and for scoped queries.
        assert!(cfg.retrieval.ann);
        assert_eq!(cfg.retrieval.ann_min_chunks, 50_000);
        assert_eq!(cfg.retrieval.rerank_backend, "llm");
        assert_eq!(
            cfg.retrieval.rerank_model,
            "mixedbread-ai/mxbai-rerank-xsmall-v1"
        );
        assert!(!cfg.parsers.audio.transcribe);
        assert!(!cfg.parsers.image.caption);
        // Caption model falls back to the default vision model when unset.
        assert_eq!(cfg.parsers.image.caption_model(), DEFAULT_CAPTION_MODEL);
        let custom = ImageParserConfig {
            caption: true,
            model: Some("moondream".to_owned()),
        };
        assert_eq!(custom.caption_model(), "moondream");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let cfg = load(Path::new(
            "/tmp/definitely-does-not-exist-indexa-config.toml",
        ))
        .unwrap();
        assert_eq!(cfg.embedding.provider, "ollama");
    }

    #[test]
    fn partial_config_merges_with_defaults() {
        let toml = r#"
[embedding]
model = "nomic-embed-text:v1.5"
dim = 768

[retrieval]
top_k = 20
rerank_model = "mixedbread-ai/mxbai-rerank-base-v1"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.embedding.model, "nomic-embed-text:v1.5");
        assert_eq!(cfg.retrieval.top_k, 20);
        // A configured rerank_model round-trips (the knob is real, not just a default).
        assert_eq!(
            cfg.retrieval.rerank_model,
            "mixedbread-ai/mxbai-rerank-base-v1"
        );
        // Fields not specified fall back to struct defaults.
        assert_eq!(cfg.retrieval.rrf_k, 60);
        assert_eq!(cfg.retrieval.rerank_backend, "llm");
        assert_eq!(cfg.describer.model, "gemma3:12b");
    }

    #[test]
    fn region_matching_picks_longest_prefix() {
        let toml = r#"
[[region]]
path = "/tmp"
[region.parsers.audio]
transcribe = true

[[region]]
path = "/tmp/voice"
[region.parsers.audio]
transcribe = false
"#;
        let cfg: Config = toml::from_str(toml).unwrap();

        let hit = cfg.region_for(Path::new("/tmp/voice/memo.m4a"));
        assert!(hit.is_some());
        // longest prefix "/tmp/voice" should win over "/tmp"
        let region = hit.unwrap();
        assert!(region.path.contains("voice"));
        let audio_transcribe = region
            .parsers
            .as_ref()
            .map(|p| p.audio.transcribe)
            .unwrap_or(false);
        assert!(!audio_transcribe); // /tmp/voice overrides /tmp

        let hit2 = cfg.region_for(Path::new("/tmp/other/file.txt"));
        assert!(hit2.is_some());
        let r2 = hit2.unwrap();
        // /tmp/other only matches /tmp
        let audio_transcribe2 = r2
            .parsers
            .as_ref()
            .map(|p| p.audio.transcribe)
            .unwrap_or(false);
        assert!(audio_transcribe2); // /tmp region has transcribe=true
    }

    #[test]
    fn chunk_strategy_roundtrips() {
        let toml = r#"[chunking]
strategy = "fixed"
size = 500
overlap = 50
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.chunking.strategy, ChunkStrategy::Fixed);
        assert_eq!(cfg.chunking.size, 500);
    }
}
