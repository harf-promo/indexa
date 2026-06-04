//! Public record types returned and consumed by the `Store` API.

#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub entry_path: String,
    pub seq: usize,
    pub heading: String,
    pub text: String,
    pub language: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub embed_model: Option<String>,
}

/// One code-relationship-graph edge (see the `edges` table). `kind` is `"imports"`
/// (then `to_ref` is a module/path) or `"defines"` (then `to_ref` is a symbol name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRecord {
    pub from_path: String,
    pub kind: String,
    pub to_ref: String,
}

/// A node in the file-to-file call graph (v0.18 signature graph).
#[derive(Debug, Clone)]
pub struct CodeGraphNode {
    pub path: String,
    /// Number of distinct files this file calls into.
    pub out_degree: usize,
    /// Number of distinct files that call into this file.
    pub in_degree: usize,
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

#[derive(Debug, Clone)]
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
    pub pending: i64,
    pub in_flight: i64,
    pub done: i64,
    pub failed: i64,
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
