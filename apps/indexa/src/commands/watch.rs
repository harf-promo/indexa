use anyhow::Result;
use indexa_core::{
    config::Config,
    pathutil::{ancestor_dirs_to_root, path_depth},
    store::{chunk_content_hash, ChunkRecord, Store},
    walker::{Entry, EntryKind},
    watcher::{self, ChangeKind, WatcherConfig},
};

use super::helpers::{build_embedder, index_db_path, resolve_roots};

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
                    if let Ok(mut store) = Store::open(&db_path_clone) {
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
                    let meta = std::fs::metadata(path).ok();
                    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let extracted = match registry.parse_guarded(path, size, max_parse_bytes) {
                        Ok(e) => e,
                        Err(_) => return,
                    };
                    if extracted.chunks.is_empty() {
                        return;
                    }

                    let chunk_records: Vec<ChunkRecord> = rt.block_on(async {
                        let mut records = Vec::with_capacity(extracted.chunks.len());
                        for chunk in &extracted.chunks {
                            let embedding = embedder.embed(&chunk.text).await.ok();
                            records.push(ChunkRecord {
                                entry_path: path.to_string_lossy().into_owned(),
                                seq: chunk.seq,
                                heading: chunk.heading.clone(),
                                text: chunk.text.clone(),
                                language: chunk.language.clone(),
                                embedding,
                                embed_model: Some(embed_model.clone()),
                                content_hash: Some(chunk_content_hash(&chunk.text)),
                            });
                        }
                        records
                    });

                    if let Ok(mut store) = Store::open(&db_path_clone) {
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
                            println!(
                                "  re-indexed: {} ({} chunks, summary re-queued)",
                                path.display(),
                                chunk_records.len()
                            );
                        }
                    }
                }
            }
        });
    })
    .await?;

    Ok(())
}
