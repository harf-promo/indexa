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
