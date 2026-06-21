//! Public record types returned and consumed by the `Store` API.

use sha2::{Digest, Sha256};

/// SHA-256 hex digest of a chunk's raw source text — the embedding cache key
/// stored in [`ChunkRecord::content_hash`]. Hash the ORIGINAL chunk text (never
/// the enriched contextual-retrieval blurb) so the cache stays valid across
/// contextual runs on the same source. Must match what `cached_embeddings_by_hash`
/// looks up, so every producer goes through this one function.
pub fn chunk_content_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub entry_path: String,
    pub seq: usize,
    pub heading: String,
    pub text: String,
    pub language: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub embed_model: Option<String>,
    /// SHA-256 hex digest of `text` (the raw source chunk, not the enriched blurb).
    /// `None` on records constructed without a hash (treated as "no cache" on upsert).
    pub content_hash: Option<String>,
}

/// One code-relationship-graph edge (see the `edges` table). `kind` is `"imports"`
/// (then `to_ref` is a module/path) or `"defines"` (then `to_ref` is a symbol name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRecord {
    pub from_path: String,
    pub kind: String,
    pub to_ref: String,
}

/// Display facts for one indexed entry row, used by `indexa inspect`.
#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub kind: String,
    pub size: u64,
    pub modified_s: Option<i64>,
}

/// A file related to a query file through the call graph, with the relation strength
/// (count of shared call→define symbols across both directions).
#[derive(Debug, Clone)]
pub struct RelatedFile {
    pub path: String,
    pub shared: usize,
}

/// A node in the file-to-file call graph (v0.18 signature graph).
#[derive(Debug, Clone)]
pub struct CodeGraphNode {
    pub path: String,
    /// Number of distinct files this file calls into.
    pub out_degree: usize,
    /// Number of distinct files that call into this file.
    pub in_degree: usize,
    /// Weighted PageRank centrality over the displayed subgraph (sums to ~1.0
    /// across all nodes; higher = more central / more of a hub). v0.20.
    pub pagerank: f64,
}

/// A directed file-to-file call edge: `from` calls a symbol that `to` defines.
#[derive(Debug, Clone)]
pub struct CodeGraphEdge {
    pub from: String,
    pub to: String,
    /// Number of distinct shared symbols (call → define) between the two files.
    pub weight: usize,
}

/// A scoped file-to-file call graph (nodes + directed edges).
#[derive(Debug, Clone)]
pub struct CodeGraph {
    pub nodes: Vec<CodeGraphNode>,
    pub edges: Vec<CodeGraphEdge>,
    /// True when the edge count hit the requested cap (more edges exist).
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub entry_path: String,
    pub seq: usize,
    pub heading: String,
    pub text: String,
    pub rrf_score: f64,
}

#[derive(Debug)]
pub struct RegionSummary {
    pub category: String,
    pub entry_count: u64,
    pub total_size: u64,
}

#[derive(Debug, Clone)]
pub struct SummaryRecord {
    pub path: String,
    pub kind: String,
    pub parent_path: Option<String>,
    pub depth: i64,
    /// L1 — the full 1–4 sentence summary.
    pub summary: String,
    /// L0 — a one-line abstract (first sentence of `summary`), for cheap scanning.
    /// `None` on rows written before tiered summaries; readers derive it on the fly.
    pub summary_l0: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub child_count: i64,
    pub byte_size: i64,
    pub model: String,
    pub source_hash: String,
    pub generated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeNode {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub child_count: i64,
    pub byte_size: i64,
    pub summary_state: Option<String>,
    /// Direct-child file count (0 for files).
    pub file_count: i64,
    /// Total chunk count for all entries under this path (0 for files).
    pub chunk_count: i64,
    /// Subtree context-coverage rollup (directory summaries only). For a dir node these count
    /// the directories at-or-under this path; for a file node all three are 0.
    /// `total` = directories in the subtree; `covered` = those whose summary is built (`done`);
    /// `partial` = those still queued (`pending`/`in_flight`). Drives the calm per-row coverage
    /// glyph (●/◐/○) and the determinate "covered/total" subtree count, replacing the old
    /// per-row pending strobe.
    pub covered: i64,
    pub partial: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// Pending rows backed by a live `entries` row — the real, processable backlog.
    pub pending: i64,
    pub in_flight: i64,
    pub done: i64,
    pub failed: i64,
    /// Pending/in-flight rows whose path is NOT a live entry (build artifacts that were
    /// later skipped, or deleted files). They can never summarize; the drain self-cleans
    /// them and `indexa prune` removes them. Surfaced so "pending" isn't inflated by them.
    pub stale: i64,
}

/// Whole-index coverage aggregates for the `status --deep` health report.
/// Chunk/summary counts join back to live `entries` rows so orphans left
/// behind by a removed root (cleaned by `prune`) can never push a coverage
/// ratio past 100%.
#[derive(Debug, Clone, Default)]
pub struct HealthStats {
    /// Entries with kind='file'.
    pub files: u64,
    /// Entries with kind='dir'.
    pub dirs: u64,
    /// File entries with at least one chunk (deep-indexed).
    pub files_with_chunks: u64,
    pub chunks: u64,
    /// Chunks with a stored embedding — anything below `chunks` is invisible
    /// to dense (and therefore hybrid) search.
    pub embedded_chunks: u64,
    /// File entries with a summaries row.
    pub files_summarized: u64,
    /// Dir entries with a summaries row.
    pub dirs_summarized: u64,
    /// Summaries generated before their entry's on-disk mtime (possibly stale).
    pub stale_summaries: u64,
}

#[derive(Debug, Clone)]
pub struct FailedQueueItem {
    pub path: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub path: String,
    pub kind: String,
    pub depth: i64,
}

/// A semantic classification of one path (the Smart-classification axis).
///
/// `source`:
/// - `auto` — an Indexa suggestion the user has not acted on yet.
/// - `user` — confirmed or corrected by the user; never overwritten by auto passes.
/// - `ignored` — a sticky tombstone for a dismissed suggestion; not re-proposed.
#[derive(Debug, Clone)]
pub struct ClassificationRecord {
    pub path: String,
    pub kind: String,
    pub category: String,
    pub confidence: f32,
    pub source: String,
    pub confirmed_at: Option<i64>,
    pub created_at: i64,
}

/// An importance weight record (v0.8).
#[derive(Debug, Clone)]
pub struct WeightRecord {
    pub target_kind: String, // "file" | "dir" | "category"
    pub target: String,      // absolute path or category name
    pub weight: f32,
    pub source: String, // "user" | "auto"
    pub reason: Option<String>,
    pub updated_at: i64,
}

/// One Decision Ledger row (v0.22) — a question Indexa asked (or a judgment it
/// recorded) plus its answer. The row fills in place on answer; revisions append
/// new rows chained via `parent_id`/`superseded_by` — see store::decisions.
///
/// `params`/`options`/`effects` are JSON kept as `String`: the store hands them
/// through verbatim and only the template/effects layers interpret them.
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    pub id: i64,
    pub decision_type: String,
    /// Stable key: a path, cluster key, or symbol.
    pub subject: String,
    pub params: String,
    pub options: String,
    /// What the automatic pass would pick (shown as the default answer).
    pub auto_value: Option<String>,
    pub chosen: Option<String>,
    /// `auto` | `user` | `system` — who supplied `chosen` (NULL while open).
    pub source: Option<String>,
    pub confidence: Option<f32>,
    /// Re-ask fingerprint: a dismissed/decided question only comes back when
    /// the evidence behind it changes.
    pub evidence_hash: String,
    pub priority: i64,
    /// `open` | `decided` | `dismissed` | `expired`.
    pub status: String,
    pub parent_id: Option<i64>,
    pub superseded_by: Option<i64>,
    pub effects: Option<String>,
    /// NULL on a decided row ⇒ projection not yet applied (repair-sweep target).
    pub effects_applied_at: Option<i64>,
    pub created_at: i64,
    pub decided_at: Option<i64>,
}

/// Input for [`Store::record_decision`] — everything a detector knows when it
/// raises a question. `paths` become `decision_paths` rows (include the subject
/// itself when it is a path; the store inserts exactly what is given).
#[derive(Debug, Clone)]
pub struct NewDecision {
    pub decision_type: String,
    pub subject: String,
    pub params: serde_json::Value,
    pub options: serde_json::Value,
    pub auto_value: Option<String>,
    pub confidence: Option<f32>,
    pub evidence_hash: String,
    pub priority: i64,
    pub paths: Vec<String>,
}

/// A named Context Pack — a user-curated set of cross-directory paths that
/// form a coherent topic (e.g. "Auth", "Tax 2025", "Client X").
#[derive(Debug, Clone)]
pub struct PackRecord {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub path_count: usize,
    pub created_at: i64,
}
