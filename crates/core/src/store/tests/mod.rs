//! Store test suite, split by concern. Shared fixtures and the import prelude live
//! here; each concern module below holds only `#[test]` functions and pulls fixtures
//! plus the store API in via `use super::*`.

pub(crate) use super::*;
pub(crate) use crate::config::HybridMode;
pub(crate) use crate::walker::{Entry, EntryKind};
pub(crate) use std::path::PathBuf;

mod basics;
mod decisions;
mod dir_apps;
mod entry_cleanup;
mod graph;
mod incremental;
mod insights;
mod packs;
mod queue;
mod scoped_resolution;
mod sessions;
mod usage;
mod weights;

// ── Shared test fixtures ──────────────────────────────────────────────────────

fn dummy_entry(path: &str, kind: EntryKind, size: u64) -> Entry {
    Entry {
        path: PathBuf::from(path),
        kind,
        size,
        modified: None,
        hint: None,
    }
}

fn dummy_chunk(path: &str, seq: usize, text: &str) -> ChunkRecord {
    ChunkRecord {
        entry_path: path.to_owned(),
        seq,
        heading: String::new(),
        text: text.to_owned(),
        language: None,
        embedding: None,
        embed_model: None,
        content_hash: None,
    }
}

/// Like [`dummy_chunk`] but carries an embedding — for tests that exercise the
/// "all chunks embedded" branch of `chunks_current_for_mtime`.
fn dummy_chunk_embedded(path: &str, seq: usize, text: &str) -> ChunkRecord {
    ChunkRecord {
        embedding: Some(vec![0.1, 0.2, 0.3]),
        embed_model: Some("test".to_owned()),
        ..dummy_chunk(path, seq, text)
    }
}

fn fts_row_count(store: &Store) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
        .unwrap()
}

fn dummy_summary(path: &str, kind: &str, parent: Option<&str>, depth: i64) -> SummaryRecord {
    SummaryRecord {
        path: path.to_owned(),
        kind: kind.to_owned(),
        parent_path: parent.map(|s| s.to_owned()),
        depth,
        summary: format!("summary of {path}"),
        summary_l0: None,
        embedding: None,
        child_count: 0,
        byte_size: 100,
        model: "gemma2:2b".to_owned(),
        source_hash: String::new(),
        generated_at: 0,
    }
}

fn edge(from: &str, kind: &str, to: &str) -> EdgeRecord {
    EdgeRecord {
        from_path: from.into(),
        kind: kind.into(),
        to_ref: to.into(),
    }
}

fn hit(path: &str, score: f64) -> SearchHit {
    SearchHit {
        chunk_id: 1,
        entry_path: path.to_owned(),
        seq: 0,
        heading: String::new(),
        text: String::new(),
        rrf_score: score,
    }
}

/// Seeded item set for the LSH near-dup tests: one cluster of near-identical
/// vectors per entry in `group_sizes`, plus `noise` unrelated random vectors.
/// Fully deterministic (SplitMix64 from `seed`) so test outcomes never flake.
fn seeded_near_dup_items(
    group_sizes: &[usize],
    noise: usize,
    dim: usize,
    seed: u64,
) -> Vec<(String, Vec<f32>)> {
    let mut rng = super::insights::SplitMix64(seed);
    let mut items = Vec::new();
    for (g, &size) in group_sizes.iter().enumerate() {
        let base: Vec<f32> = (0..dim).map(|_| rng.next_unit()).collect();
        for m in 0..size {
            // Tiny perturbation → in-group cosine ≈ 0.999, far above the 0.9
            // test threshold; random 24-dim vectors sit near cosine 0.
            let v: Vec<f32> = base.iter().map(|x| x + rng.next_unit() * 0.005).collect();
            items.push((format!("/group{g}/file{m}.txt"), v));
        }
    }
    for k in 0..noise {
        let v: Vec<f32> = (0..dim).map(|_| rng.next_unit()).collect();
        items.push((format!("/noise/file{k:04}.txt"), v));
    }
    items
}

/// Count rows still referencing `path` across every child table that delete_entry /
/// delete_subtree are responsible for clearing. importance_weights is deliberately
/// excluded (weights persist across entry removal by design).
fn orphan_rows_for(store: &Store, path: &str) -> i64 {
    let c = store.db_connection();
    let q = |sql: &str| -> i64 { c.query_row(sql, [path], |r| r.get(0)).unwrap() };
    q("SELECT COUNT(*) FROM chunks WHERE entry_path = ?1")
        + q("SELECT COUNT(*) FROM chunks_fts WHERE entry_path = ?1")
        + q("SELECT COUNT(*) FROM edges WHERE from_path = ?1")
        + q("SELECT COUNT(*) FROM summaries WHERE path = ?1")
        + q("SELECT COUNT(*) FROM summary_queue WHERE path = ?1")
        + q("SELECT COUNT(*) FROM classifications WHERE path = ?1")
        + q("SELECT COUNT(*) FROM directory_apps WHERE path = ?1")
}

/// Populate every entry-keyed child table for `path`, then return the store.
fn seed_full_entry(store: &mut Store, path: &str) {
    store
        .upsert_entries(&[dummy_entry(path, EntryKind::File, 100)])
        .unwrap();
    store
        .upsert_chunks(&[dummy_chunk(path, 0, "hello world")])
        .unwrap();
    store
        .upsert_summary(&dummy_summary(path, "file", Some("/"), 1))
        .unwrap();
    store
        .upsert_edges(&[
            edge(path, "imports", "std::fs"),
            edge(path, "defines", "run"),
        ])
        .unwrap();
    store
        .enqueue_summary_items(&[(path.to_owned(), "file".to_owned(), 1)])
        .unwrap();
    store
        .upsert_auto_classifications(&[(path.into(), "file".into(), "code".into(), 0.9)])
        .unwrap();
    store
        .replace_apps_for_dir(
            path,
            &[DetectedApp {
                path: path.to_owned(),
                app_kind: "rust_crate".to_owned(),
                app_name: "Rust crate".to_owned(),
                family: "code".to_owned(),
                specificity: 10,
                is_primary: true,
                markers_json: "[\"Cargo.toml\"]".to_owned(),
                source: "builtin".to_owned(),
                detected_at: 0,
            }],
        )
        .unwrap();
}

/// A minimal open question; tests override fields as needed.
fn new_decision(decision_type: &str, subject: &str) -> NewDecision {
    NewDecision {
        decision_type: decision_type.to_owned(),
        subject: subject.to_owned(),
        params: serde_json::json!({}),
        options: serde_json::json!(["code", "docs"]),
        auto_value: Some("code".to_owned()),
        confidence: Some(0.7),
        evidence_hash: "h1".to_owned(),
        priority: 50,
        paths: vec![subject.to_owned()],
    }
}

fn entry_with_mtime(path: &str, kind: EntryKind, mtime_secs: u64) -> Entry {
    Entry {
        modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs)),
        ..dummy_entry(path, kind, 1)
    }
}
