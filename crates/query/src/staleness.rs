//! Per-citation staleness attestation (1.2): does a cited file's index reflect its current
//! on-disk content? Borrowed from codebase-memory-mcp's generation/coverage stamps — extends
//! Indexa's existing freshness story (hash-gated incremental re-summarize) to per-answer
//! granularity, so retrieval finally admits when it may be serving stale text.
//!
//! Annotation-only: never changes retrieval scores or which chunks are cited, so it needs no
//! `indexa eval` gate. Fail-open — any I/O error (file moved/deleted/permission) reports "not
//! stale" rather than raising an error, since staleness here is advisory, not authoritative.

use indexa_core::store::Store;
use std::collections::HashSet;
use std::time::UNIX_EPOCH;

/// True if `path`'s on-disk mtime is newer than what's indexed — its chunks were last
/// (re-)indexed before the file's current mtime, or some chunk is missing an embedding.
/// Reuses [`Store::chunks_current_for_mtime`] (the same check `deep`'s skip-if-current logic
/// uses), just with a freshly-read mtime instead of a walk-time one.
pub fn is_stale(store: &Store, path: &str) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let mtime_secs = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    !store
        .chunks_current_for_mtime(path, mtime_secs)
        .unwrap_or(true)
}

/// Staleness for a batch of cited paths (deduplicated): the set of stale paths (for a fast
/// per-citation membership check) plus `(stale_count, total_count)` for a footer summary.
pub fn stale_paths<'a>(
    store: &Store,
    paths: impl IntoIterator<Item = &'a str>,
) -> (HashSet<String>, usize, usize) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stale: HashSet<String> = HashSet::new();
    for p in paths {
        if !seen.insert(p.to_owned()) {
            continue;
        }
        if is_stale(store, p) {
            stale.insert(p.to_owned());
        }
    }
    let total = seen.len();
    let stale_count = stale.len();
    (stale, stale_count, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::store::ChunkRecord;

    fn dummy_chunk_embedded(path: &str, with_embedding: bool) -> ChunkRecord {
        ChunkRecord {
            entry_path: path.to_owned(),
            seq: 0,
            heading: String::new(),
            text: "hello world".to_owned(),
            language: None,
            embedding: with_embedding.then(|| vec![0.1, 0.2, 0.3]),
            embed_model: with_embedding.then(|| "test".to_owned()),
            content_hash: None,
        }
    }

    /// `upsert_chunks` stamps `indexed_at` from the DB's own `unixepoch()` at insert time —
    /// there is no writer-controlled field for it. Override it directly so tests can place a
    /// chunk's indexed_at clearly before/after a real file's disk mtime without a flaky
    /// same-second race or a new `filetime` test dependency.
    fn set_indexed_at(store: &Store, path: &str, at: i64) {
        store
            .db_connection()
            .execute(
                "UPDATE chunks SET indexed_at = ?1 WHERE entry_path = ?2",
                rusqlite::params![at, path],
            )
            .unwrap();
    }

    #[test]
    fn is_stale_false_for_missing_file() {
        // Fail-open: a moved/deleted cited file is never flagged stale (nothing to compare).
        let store = Store::open_in_memory().unwrap();
        assert!(!is_stale(&store, "/does/not/exist.rs"));
    }

    #[test]
    fn is_stale_true_when_disk_mtime_newer_than_indexed_at() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let path = file.to_str().unwrap();

        // Indexed "long ago" (indexed_at = 1); disk mtime is now (>> 1) — stale.
        store
            .upsert_chunks(&[dummy_chunk_embedded(path, true)])
            .unwrap();
        set_indexed_at(&store, path, 1);
        assert!(is_stale(&store, path));

        // Indexed with a far-future timestamp — never stale relative to disk.
        set_indexed_at(&store, path, i64::MAX / 2);
        assert!(!is_stale(&store, path));
    }

    #[test]
    fn is_stale_true_when_embedding_missing() {
        // A chunk stored without an embedding never counts as current, even with a fresh
        // indexed_at — mirrors chunks_current_for_mtime's own contract.
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let path = file.to_str().unwrap();
        store
            .upsert_chunks(&[dummy_chunk_embedded(path, false)])
            .unwrap();
        set_indexed_at(&store, path, i64::MAX / 2);
        assert!(is_stale(&store, path));
    }

    #[test]
    fn stale_paths_dedupes_and_summarizes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let fresh = dir.path().join("fresh.rs");
        let old = dir.path().join("old.rs");
        std::fs::write(&fresh, "fresh").unwrap();
        std::fs::write(&old, "old").unwrap();
        let fresh_s = fresh.to_str().unwrap();
        let old_s = old.to_str().unwrap();
        store
            .upsert_chunks(&[
                dummy_chunk_embedded(fresh_s, true),
                dummy_chunk_embedded(old_s, true),
            ])
            .unwrap();
        set_indexed_at(&store, fresh_s, i64::MAX / 2);
        set_indexed_at(&store, old_s, 1);

        let (stale, stale_count, total) = stale_paths(&store, [fresh_s, old_s, old_s /* dup */]);
        assert_eq!(total, 2, "duplicate path counted once");
        assert_eq!(stale_count, 1);
        assert!(stale.contains(old_s));
        assert!(!stale.contains(fresh_s));
    }
}
