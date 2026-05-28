//! Configuration loader for `~/.indexa/config.toml`.
//!
//! All fields have sensible defaults — a missing or empty config file is valid.
//! Unknown keys are silently ignored (deny_unknown_fields is off) so older config
//! files stay compatible with newer binaries.

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
    /// Per-directory overrides. Matched by path prefix (longest wins).
    #[serde(default)]
    pub region: Vec<RegionConfig>,
    /// Optional cloud-provider API keys persisted to config.toml.
    #[serde(default)]
    pub api_keys: ApiKeysConfig,
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
    /// Weighted linear combination.
    Weighted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
    pub hybrid: HybridMode,
    /// RRF rank constant k (60 is the standard default).
    pub rrf_k: usize,
    /// Number of results to retrieve before reranking.
    pub top_k: usize,
    /// Enable cross-encoder reranking (adds ~200ms; requires a reranker model).
    pub rerank: bool,
    /// Weight of summary hits relative to chunk hits in RRF fusion (0.0 = disabled).
    pub summary_weight: f32,
    /// Depth-boost coefficient α: parent summaries score 1 + α*(max_depth - depth) higher.
    pub summary_depth_alpha: f32,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            hybrid: HybridMode::Rrf,
            rrf_k: 60,
            top_k: 8,
            rerank: false,
            summary_weight: 0.5,
            summary_depth_alpha: 0.15,
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
    /// Enable Anthropic-style per-chunk contextual prefix at index time.
    pub contextual_retrieval: bool,
    /// Model for per-file summarization (smaller/faster is fine).
    pub file_model: String,
    /// Model for directory roll-up summaries (stronger model recommended).
    pub dir_model: String,
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
}

impl Default for DescriberConfig {
    fn default() -> Self {
        Self {
            provider: "ollama".into(),
            model: "gemma3:12b".into(),
            base_url: "http://localhost:11434".into(),
            contextual_retrieval: false,
            file_model: "gemma3:4b".into(),
            dir_model: "gemma3:12b".into(),
            mode: SummaryMode::Augment,
            queue_concurrency: 2,
            max_children_per_summary: 30,
            passes_first: 2,
            passes_refresh: 1,
            passes_cap: 3,
        }
    }
}

// ── Parser overrides ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ParsersConfig {
    pub pdf: PdfParserConfig,
    pub image: ImageParserConfig,
    pub audio: AudioParserConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfParserConfig {
    /// "pdfium" (default) | "marker" (better for scanned/complex PDFs, requires Marker CLI)
    pub backend: String,
}

impl Default for PdfParserConfig {
    fn default() -> Self {
        Self {
            backend: "pdfium".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageParserConfig {
    /// Set true to enable SigLIP-2 / vision-model captioning (opt-in).
    pub caption: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioParserConfig {
    /// Set true to enable whisper.cpp transcription (opt-in).
    pub transcribe: bool,
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
    std::fs::write(path, &text).with_context(|| format!("writing config: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("setting permissions on {}", path.display()))?;
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
    fn default_config_is_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.embedding.model, "nomic-embed-text");
        assert_eq!(cfg.embedding.dim, 768);
        assert_eq!(cfg.retrieval.rrf_k, 60);
        assert!(!cfg.parsers.audio.transcribe);
        assert!(!cfg.parsers.image.caption);
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
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.embedding.model, "nomic-embed-text:v1.5");
        assert_eq!(cfg.retrieval.top_k, 20);
        // Fields not specified fall back to struct defaults.
        assert_eq!(cfg.retrieval.rrf_k, 60);
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
