use anyhow::Result;
use indexa_core::{
    config::Config,
    pathutil::{ancestor_dirs_to_root, path_depth},
    store::{chunk_content_hash, ChunkRecord, Store},
    walker::{Entry, EntryKind},
    watcher::{self, ChangeKind, WatcherConfig},
};

use super::helpers::{build_embedder, index_db_path, resolve_roots};

/// Embed one file's parsed chunks into [`ChunkRecord`]s, returning the records plus how many
/// of them ended up WITHOUT a vector.
///
/// Split out of the watch closure for two reasons. It is the seam that makes the
/// embed-failure path testable at all (the closure it came from needs a live notify
/// debouncer), and its absence is why this path silently diverged from the web server's
/// equivalent in `crates/web/src/handlers/watch.rs`, which has warned on the same failure
/// for a while. Failing open is deliberate and unchanged — an unembedded chunk is still
/// stored and still findable by BM25 — but it must not be *silent*: without a vector that
/// file drops out of dense retrieval entirely, and nothing anywhere said so.
async fn build_chunk_records(
    embedder: &(dyn indexa_embed::Embedder + Send + Sync),
    extracted: &indexa_parsers::types::Extracted,
    path: &std::path::Path,
    embed_model: &str,
    redact_at_index: bool,
) -> (Vec<ChunkRecord>, usize) {
    let mut records = Vec::with_capacity(extracted.chunks.len());
    let mut degraded = 0usize;
    for chunk in &extracted.chunks {
        let embedding = match embedder.embed(&chunk.text).await {
            Ok(e) => Some(e),
            Err(e) => {
                degraded += 1;
                // Mirrors the web watcher's warning verbatim in intent: the chunk is still
                // stored (searchable via BM25), but without a vector it won't match dense
                // retrieval. Surface it — a silently-unembedded chunk degrades search
                // invisibly.
                tracing::warn!(
                    path = %path.display(),
                    seq = chunk.seq,
                    error = %e,
                    "watch: embedding failed; chunk stored without a vector"
                );
                None
            }
        };
        records.push(ChunkRecord {
            entry_path: path.to_string_lossy().into_owned(),
            seq: chunk.seq,
            heading: chunk.heading.clone(),
            text: indexa_query::redact::chunk_text_for_store(&chunk.text, redact_at_index),
            language: chunk.language.clone(),
            embedding,
            embed_model: Some(embed_model.to_owned()),
            content_hash: Some(chunk_content_hash(&chunk.text)),
        });
    }
    (records, degraded)
}

pub(crate) async fn cmd_watch(
    paths: Vec<String>,
    embed_model_flag: Option<String>,
    cfg: &Config,
) -> Result<()> {
    let roots = resolve_roots(paths, false)?;
    let db_path = index_db_path()?;

    let embed_model = embed_model_flag
        .as_deref()
        .unwrap_or(&cfg.embedding.model)
        .to_owned();

    let embedder = build_embedder(cfg, Some(&embed_model))?;

    println!(
        "Watching {} path(s) for changes. Press Ctrl-C to stop.",
        roots.len()
    );
    for r in &roots {
        println!("  {}", r.display());
    }
    println!();

    let session = watcher::watch(&roots, &WatcherConfig::default())?;

    let db_path_clone = db_path.clone();
    let max_parse_bytes = cfg.parsers.max_file_mb.saturating_mul(1024 * 1024);
    // Chunk-aware registry, built before the (`'static`) watch closure so it can be moved in and
    // reused for every event, honoring `[chunking]` size/overlap.
    let registry = super::helpers::chunk_registry(cfg);
    // `resolve_roots` already returns canonical (verbatim-stripped) roots, which match
    // notify's canonical event paths — so the ancestor-walk `starts_with` check works
    // without re-canonicalizing here (which on Windows would re-add the `\\?\` prefix).
    let watch_roots = roots.clone();
    // Apply the same file-selection policy the scan walker uses, per event: skip build artifacts /
    // sensitive dirs / oversized files / `[scan] ignore`+gitignore matches. Built once, moved in.
    let scan_matchers = indexa_core::walker::build_scan_matchers(
        &roots,
        cfg.scan.respect_gitignore,
        &cfg.scan.ignore,
        cfg.scan.custom_ignore,
    );
    let include_sensitive = cfg.scan.include_sensitive;
    let redact_at_index = cfg.scan.redact_at_index;
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();

        watcher::run_watch_loop(session, |event| {
            let path = &event.path;
            if path.is_dir() {
                return;
            }
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false)
            {
                return;
            }

            match event.kind {
                ChangeKind::Remove => {
                    let mut store = match Store::open(&db_path_clone) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "watch: could not open the index; this deletion was NOT applied"
                            );
                            return;
                        }
                    };
                    {
                        let path_str = path.to_string_lossy().into_owned();
                        // Full removal — `delete_chunks_for` left the file's summary, queue,
                        // and entry rows behind, so search/browse kept returning a file that
                        // no longer exists. `delete_entry` clears chunks + FTS + summary +
                        // queue + classification + entry in one transaction.
                        if let Err(e) = store.delete_entry(&path_str) {
                            tracing::warn!("failed to remove {path_str}: {e}");
                        } else {
                            // The dead file's ancestor roll-ups must refresh without it.
                            for dir in ancestor_dirs_to_root(path, &watch_roots) {
                                let dir_str = dir.to_string_lossy().into_owned();
                                if let Err(e) =
                                    store.mark_for_resummary(&dir_str, "dir", path_depth(&dir_str))
                                {
                                    tracing::warn!("failed to re-queue roll-up for {dir_str}: {e}");
                                }
                            }
                            println!("  removed: {path_str}");
                        }
                    }
                }
                ChangeKind::Upsert => {
                    // Don't re-index build artifacts / sensitive files / oversized blobs / ignored
                    // paths — the scan walker skips them, so a live watch must too.
                    if !indexa_core::walker::should_index_file(
                        path,
                        &watch_roots,
                        include_sensitive,
                        Some(indexa_core::walker::DEFAULT_MAX_FILESIZE),
                        &scan_matchers,
                    ) {
                        return;
                    }
                    let meta = std::fs::metadata(path).ok();
                    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let extracted = match registry.parse_guarded(path, size, max_parse_bytes) {
                        Ok(e) => e,
                        Err(_) => return,
                    };
                    if extracted.chunks.is_empty() {
                        return;
                    }

                    let (chunk_records, degraded) = rt.block_on(build_chunk_records(
                        embedder.as_ref(),
                        &extracted,
                        path,
                        &embed_model,
                        redact_at_index,
                    ));

                    let mut store = match Store::open(&db_path_clone) {
                        Ok(s) => s,
                        Err(e) => {
                            // Previously an `if let Ok(...)` with no `else`: a locked,
                            // corrupted or permission-denied index silently dropped this
                            // file's update on the floor, with no log, no retry and nothing
                            // for the user to notice.
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "watch: could not open the index; this change was NOT indexed"
                            );
                            return;
                        }
                    };
                    {
                        // A newly-created file has no `entries` row (only `scan` writes those), so
                        // without this its chunks are orphans: never summarized (mark_for_resummary
                        // skips entry-less paths) and wiped by the next `prune`. upsert_entries is an
                        // idempotent ON-CONFLICT upsert, so it also refreshes size/mtime on edits.
                        let entry = Entry {
                            path: path.to_path_buf(),
                            kind: EntryKind::File,
                            size,
                            modified: meta.as_ref().and_then(|m| m.modified().ok()),
                            hint: indexa_core::surface::classify(path)
                                .or_else(|| indexa_core::surface::classify_file_by_extension(path)),
                            is_binary: false,
                        };
                        if let Err(e) = store.upsert_entries(&[entry]) {
                            tracing::warn!("failed to upsert entry for {}: {e}", path.display());
                        }
                        if let Err(e) = store.upsert_chunks(&chunk_records) {
                            tracing::warn!("failed to upsert chunks for {}: {e}", path.display());
                        } else {
                            // Re-embedding alone leaves the summary stale. Re-queue this file
                            // and every ancestor roll-up so the background worker refreshes them.
                            // `watch` itself only embeds + enqueues — run `indexa worker` (or
                            // click "Regenerate" / "Rebuild all" in the web UI) to drain the queue
                            // and actually regenerate the summaries. `indexa serve` does NOT
                            // drain the queue automatically; only explicit jobs do.
                            // (`mark_for_resummary` skips an item a worker is already summarizing,
                            // so an edit landing during that window is picked up by the next edit
                            // or a later `deep`/`summarize` rather than double-claimed.)
                            let path_str = path.to_string_lossy().into_owned();
                            if let Err(e) =
                                store.mark_for_resummary(&path_str, "file", path_depth(&path_str))
                            {
                                tracing::warn!("failed to re-queue summary for {path_str}: {e}");
                            }
                            for dir in ancestor_dirs_to_root(path, &watch_roots) {
                                let dir_str = dir.to_string_lossy().into_owned();
                                if let Err(e) =
                                    store.mark_for_resummary(&dir_str, "dir", path_depth(&dir_str))
                                {
                                    tracing::warn!("failed to re-queue roll-up for {dir_str}: {e}");
                                }
                            }
                            // The degraded count is reported per file rather than as a
                            // session total: `run_watch_loop` blocks until the process is
                            // signalled, so a "totals on shutdown" line would never print.
                            if degraded > 0 {
                                println!(
                                    "  re-indexed: {} ({} chunks, {degraded} without embeddings \
                                     — keyword-only until the embedder is reachable and you \
                                     re-run `indexa deep`; summary re-queued)",
                                    path.display(),
                                    chunk_records.len()
                                );
                            } else {
                                println!(
                                    "  re-indexed: {} ({} chunks, summary re-queued)",
                                    path.display(),
                                    chunk_records.len()
                                );
                            }
                        }
                    }
                }
            }
        });
    })
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_parsers::types::{Chunk, Extracted};

    /// Embeds nothing, ever — stands in for an Ollama that is down, unreachable, or has had
    /// the model pulled out from under it, which is precisely when a watch quietly stops
    /// producing vectors.
    struct FailingEmbedder;

    #[async_trait::async_trait]
    impl indexa_embed::Embedder for FailingEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("connection refused")
        }
        fn dim(&self) -> usize {
            3
        }
    }

    struct OkEmbedder;

    #[async_trait::async_trait]
    impl indexa_embed::Embedder for OkEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1, 0.2, 0.3])
        }
        fn dim(&self) -> usize {
            3
        }
    }

    fn extracted(n: usize) -> Extracted {
        Extracted {
            source: std::path::PathBuf::from("/r/a.rs"),
            mime: "text/x-rust".to_owned(),
            chunks: (0..n)
                .map(|seq| Chunk {
                    source: std::path::PathBuf::from("/r/a.rs"),
                    seq,
                    heading: String::new(),
                    text: format!("chunk {seq}"),
                    language: Some("rust".to_owned()),
                })
                .collect(),
            edges: vec![],
        }
    }

    #[tokio::test]
    async fn embed_failures_are_counted_and_the_chunk_is_still_stored() {
        // Failing open is the correct behavior and must not change: the chunk is still
        // stored so BM25 can find it. What changed is that the failure is no longer
        // invisible — `degraded` is what the caller reports, and a `tracing::warn!` fires
        // per chunk. Before this, `.await.ok()` threw the error away entirely and the file
        // silently dropped out of dense retrieval.
        let (records, degraded) = build_chunk_records(
            &FailingEmbedder,
            &extracted(3),
            std::path::Path::new("/r/a.rs"),
            "test-model",
            false,
        )
        .await;
        assert_eq!(records.len(), 3, "every chunk is still stored");
        assert_eq!(degraded, 3, "every chunk is reported as unembedded");
        assert!(
            records.iter().all(|r| r.embedding.is_none()),
            "a failed embed must store no vector, not a bogus one"
        );
        assert!(
            records.iter().all(|r| r.content_hash.is_some()),
            "the content hash is independent of embedding success"
        );
    }

    #[tokio::test]
    async fn a_successful_embed_reports_zero_degraded() {
        let (records, degraded) = build_chunk_records(
            &OkEmbedder,
            &extracted(2),
            std::path::Path::new("/r/a.rs"),
            "test-model",
            false,
        )
        .await;
        assert_eq!(records.len(), 2);
        assert_eq!(degraded, 0);
        assert!(records.iter().all(|r| r.embedding.is_some()));
        assert_eq!(records[1].seq, 1, "chunk order is preserved");
    }

    #[tokio::test]
    async fn redaction_is_applied_to_stored_text_when_enabled() {
        let mut ex = extracted(1);
        ex.chunks[0].text = "aws_key = AKIAIOSFODNN7EXAMPLE".to_owned();
        let (records, _) = build_chunk_records(
            &OkEmbedder,
            &ex,
            std::path::Path::new("/r/a.rs"),
            "test-model",
            true,
        )
        .await;
        assert!(
            !records[0].text.contains("AKIAIOSFODNN7EXAMPLE"),
            "redact_at_index must scrub before the record is stored: {}",
            records[0].text
        );
    }
}
