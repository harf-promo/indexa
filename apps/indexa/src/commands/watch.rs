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

/// Persist one re-parsed file: entry row, chunks, code graph, then (when the chunks landed)
/// re-queue its summary and every ancestor roll-up. Split out of the watch closure (which
/// needs a live notify debouncer) so the persistence contract is testable. Returns the
/// Any chunk/graph persistence error aborts before summary queue updates; entry-upsert failures
/// retain the existing warning behavior.
fn persist_upsert(
    store: &mut Store,
    path: &std::path::Path,
    watch_roots: &[std::path::PathBuf],
    entry: Entry,
    chunk_records: &[ChunkRecord],
    edges: &[indexa_parsers::types::Edge],
) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    // A newly-created file has no `entries` row (only `scan` writes those), so without this
    // its chunks are orphans: never summarized (mark_for_resummary skips entry-less paths)
    // and wiped by the next `prune`. upsert_entries is an idempotent ON-CONFLICT upsert, so
    // it also refreshes size/mtime on edits.
    if let Err(e) = store.upsert_entries(&[entry]) {
        tracing::warn!("failed to upsert entry for {}: {e}", path.display());
    }
    // `deep` skips a file whose chunks are current, so watch must replace chunks and graph
    // together. Any failure in either half rolls the transaction back before summary work.
    let (edge_records, symbol_records) =
        indexa_query::code_graph::code_graph_records(&path_str, edges);
    store.replace_file_watch_content(&path_str, chunk_records, &edge_records, &symbol_records)?;
    // Re-embedding alone leaves the summary stale. Re-queue this file and every ancestor
    // roll-up so the background worker refreshes them. `watch` itself only embeds + enqueues
    // — run `indexa worker` (or click "Regenerate" / "Rebuild all" in the web UI) to drain the
    // queue and actually regenerate the summaries. `indexa serve` does NOT drain the queue
    // automatically; only explicit jobs do. (`mark_for_resummary` skips an item a worker is
    // already summarizing, so an edit landing during that window is picked up by the next
    // edit or a later `deep`/`summarize` rather than double-claimed.)
    if let Err(e) = store.mark_for_resummary(&path_str, "file", path_depth(&path_str)) {
        tracing::warn!("failed to re-queue summary for {path_str}: {e}");
    }
    for dir in ancestor_dirs_to_root(path, watch_roots) {
        let dir_str = dir.to_string_lossy().into_owned();
        if let Err(e) = store.mark_for_resummary(&dir_str, "dir", path_depth(&dir_str)) {
            tracing::warn!("failed to re-queue roll-up for {dir_str}: {e}");
        }
    }
    Ok(())
}

/// Parse a stable snapshot of the file opened by the watch event. Individual parsers accept
/// paths and reopen them, so copy from the no-follow, nonblocking descriptor before dispatching.
fn parse_watch_file(
    registry: &indexa_parsers::registry::Registry,
    path: &std::path::Path,
    max_parse_bytes: u64,
) -> Result<(indexa_parsers::types::Extracted, std::fs::Metadata)> {
    use std::io::Write;

    let mut source = indexa_core::walker::open_regular_file(path)?;
    let metadata = source.metadata()?;
    let size = metadata.len();
    if max_parse_bytes > 0 && size > max_parse_bytes {
        anyhow::bail!("file exceeds watch parse size limit");
    }
    let suffix = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();
    let mut snapshot = tempfile::Builder::new().suffix(&suffix).tempfile()?;
    std::io::copy(&mut source, &mut snapshot)?;
    snapshot.flush()?;
    Ok((
        registry.parse_guarded(snapshot.path(), size, max_parse_bytes)?,
        metadata,
    ))
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
            // A directory upsert carries no content (the watcher follows it with one event per
            // file), but a directory *remove* must still clear its subtree: the watcher emits one
            // for a directory that a rename swapped out while the path exists again.
            if event.kind == ChangeKind::Upsert && path.is_dir() {
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
                        // no longer exists. `delete_subtree` also covers a renamed directory's
                        // descendants, clearing every child table in one transaction.
                        if let Err(e) = store.delete_subtree(&path_str) {
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
                    let (extracted, meta) = match parse_watch_file(&registry, path, max_parse_bytes)
                    {
                        Ok(e) => e,
                        Err(_) => return,
                    };

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
                        let entry = Entry {
                            path: path.to_path_buf(),
                            kind: EntryKind::File,
                            size: meta.len(),
                            modified: meta.modified().ok(),
                            hint: indexa_core::surface::classify(path)
                                .or_else(|| indexa_core::surface::classify_file_by_extension(path)),
                            is_binary: false,
                        };
                        if let Err(e) = persist_upsert(
                            &mut store,
                            path,
                            &watch_roots,
                            entry,
                            &chunk_records,
                            &extracted.edges,
                        ) {
                            tracing::warn!("failed to upsert chunks for {}: {e}", path.display());
                        } else {
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

    #[cfg(target_os = "linux")]
    #[test]
    fn watch_parse_skips_fifo_swapped_after_discovery_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.txt");
        std::fs::write(&path, "regular file discovered first").unwrap();
        assert!(std::fs::metadata(&path).unwrap().is_file());
        std::fs::remove_file(&path).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = parse_watch_file(&indexa_parsers::registry::Registry::new(), &path, 0);
            let _ = tx.send(result);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("watch parse blocked on a FIFO swapped in after discovery");
        assert!(result.is_err(), "FIFO replacement must be skipped");
    }

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

    fn defines(name: &str, range: (usize, usize)) -> indexa_parsers::types::Edge {
        indexa_parsers::types::Edge {
            from: std::path::PathBuf::from("/r/a.rs"),
            kind: "defines",
            to: name.to_owned(),
            symbol_kind: Some("fn"),
            line_range: Some(range),
        }
    }

    /// H3: a watch upsert must refresh the code graph, not just the chunks. `deep` skips a
    /// file whose chunks are current, so without this the stale symbol range was permanent.
    #[test]
    fn watch_upsert_refreshes_the_code_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("a.rs");
        std::fs::write(&file, "fn run() {}\n").unwrap();
        let mut store = Store::open(&dir.path().join("idx.db")).unwrap();
        let entry = || Entry {
            path: file.clone(),
            kind: EntryKind::File,
            size: 12,
            modified: None,
            hint: None,
            is_binary: false,
        };
        let path_str = file.to_string_lossy().into_owned();
        let roots = vec![root.clone()];

        persist_upsert(
            &mut store,
            &file,
            &roots,
            entry(),
            &[],
            &[defines("run", (1, 3))],
        )
        .unwrap();
        let syms = store.symbols_in_file(&path_str).unwrap();
        assert_eq!((syms[0].start_line, syms[0].end_line), (1, 3));

        // The symbol moves and a second one appears.
        persist_upsert(
            &mut store,
            &file,
            &roots,
            entry(),
            &[],
            &[defines("run", (10, 14)), defines("other", (16, 18))],
        )
        .unwrap();
        let mut syms = store.symbols_in_file(&path_str).unwrap();
        syms.sort_by_key(|s| s.start_line);
        assert_eq!(syms.len(), 2, "stale rows replaced, not appended: {syms:?}");
        assert_eq!((syms[0].name.as_str(), syms[0].start_line), ("run", 10));
        assert_eq!((syms[1].name.as_str(), syms[1].start_line), ("other", 16));
    }

    #[test]
    fn watch_chunk_and_graph_replacement_rolls_back_after_injected_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("idx.db")).unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        let roots = vec![dir.path().to_owned()];
        let old = ChunkRecord {
            entry_path: path.clone(),
            seq: 0,
            heading: String::new(),
            text: "old chunk".into(),
            language: None,
            embedding: None,
            embed_model: None,
            content_hash: None,
        };
        let old_edge = indexa_core::store::EdgeRecord {
            from_path: path.clone(),
            kind: "defines".into(),
            to_ref: "old".into(),
        };
        let old_symbol = indexa_core::store::SymbolRecord {
            path: path.clone(),
            name: "old".into(),
            kind: "fn".into(),
            start_line: 1,
            end_line: 1,
        };
        store.upsert_chunks(&[old]).unwrap();
        store
            .replace_file_code_graph(&path, &[old_edge], &[old_symbol])
            .unwrap();
        let new = ChunkRecord {
            entry_path: path.clone(),
            seq: 0,
            heading: String::new(),
            text: "new chunk".into(),
            language: None,
            embedding: None,
            embed_model: None,
            content_hash: None,
        };
        let new_graph_edge = indexa_parsers::types::Edge {
            from: file.clone(),
            kind: "defines",
            to: "new".into(),
            symbol_kind: Some("fn"),
            line_range: Some((2, 2)),
        };
        let entry = || Entry {
            path: file.clone(),
            kind: EntryKind::File,
            size: 12,
            modified: None,
            hint: None,
            is_binary: false,
        };
        // This trigger injects a graph-write failure after the chunk write begins.
        store.db_connection().execute_batch(
            "CREATE TRIGGER fail_watch_graph BEFORE INSERT ON edges BEGIN SELECT RAISE(ABORT, 'injected graph failure'); END;",
        ).unwrap();
        let err = persist_upsert(
            &mut store,
            &file,
            &roots,
            entry(),
            &[new],
            &[new_graph_edge],
        );
        assert!(err.is_err());
        assert_eq!(
            store.chunks_for_path(&path, 10).unwrap()[0].text,
            "old chunk"
        );
        assert_eq!(store.symbols_in_file(&path).unwrap()[0].name, "old");
        assert_eq!(store.all_edges().unwrap()[0].to_ref, "old");
    }
}
