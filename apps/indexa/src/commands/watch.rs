use anyhow::Result;
use indexa_core::{
    config::Config,
    store::{ChunkRecord, Store},
    watcher::{self, ChangeKind, WatcherConfig},
};
use std::path::{Path, PathBuf};

use super::helpers::{build_embedder, index_db_path, resolve_roots};

/// Path's `/`-or-`\`-separator count — the same depth metric `enqueue_subtree`
/// uses, so re-queued items sort correctly (deepest first) in `next_queue_item`.
fn path_depth(path: &str) -> i64 {
    path.chars().filter(|&c| c == '/' || c == '\\').count() as i64
}

/// The ancestor directories of `path`, from its immediate parent up to and
/// including the watched root that contains it. A changed file makes every
/// roll-up on this chain stale, so each is re-queued for the worker.
fn ancestor_dirs_to_root(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(root) = roots.iter().find(|r| path.starts_with(r)) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut cur = path.parent();
    while let Some(d) = cur {
        // Stay within the watched subtree: stop if we've walked above the root.
        // (Also makes a file-as-root degenerate cleanly to no ancestor dirs.)
        if !d.starts_with(root) {
            break;
        }
        dirs.push(d.to_path_buf());
        if d == root.as_path() {
            break;
        }
        cur = d.parent();
    }
    dirs
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
    // Canonicalize so the ancestor walk's root match works: notify reports event
    // paths in canonical form (e.g. macOS /tmp → /private/tmp), which would not
    // `starts_with` a symlinked watched root. Falls back to the raw path if the
    // root can't be canonicalized.
    let watch_roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
        .collect();
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
                        if let Err(e) = store.delete_chunks_for(&path_str) {
                            tracing::warn!("failed to delete chunks for {path_str}: {e}");
                        } else {
                            println!("  removed: {path_str}");
                        }
                    }
                }
                ChangeKind::Upsert => {
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    let extracted = match indexa_parsers::registry::parse_guarded(
                        path,
                        size,
                        max_parse_bytes,
                    ) {
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
                            });
                        }
                        records
                    });

                    if let Ok(mut store) = Store::open(&db_path_clone) {
                        if let Err(e) = store.upsert_chunks(&chunk_records) {
                            tracing::warn!("failed to upsert chunks for {}: {e}", path.display());
                        } else {
                            // Re-embedding alone leaves the summary stale. Re-queue this file
                            // and every ancestor roll-up so the background worker refreshes them.
                            // `watch` itself only embeds + enqueues — run `indexa worker` (or
                            // `serve`) to drain the queue and actually regenerate the summaries.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_depth_counts_separators() {
        assert_eq!(path_depth("/a/b/c.txt"), 3);
        assert_eq!(path_depth("/a"), 1);
        assert_eq!(path_depth("rel"), 0);
    }

    #[test]
    fn ancestor_dirs_walks_up_to_and_includes_root() {
        let roots = vec![PathBuf::from("/proj")];
        let dirs = ancestor_dirs_to_root(Path::new("/proj/src/mod/file.rs"), &roots);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/proj/src/mod"),
                PathBuf::from("/proj/src"),
                PathBuf::from("/proj"),
            ]
        );
    }

    #[test]
    fn ancestor_dirs_file_directly_in_root() {
        let roots = vec![PathBuf::from("/proj")];
        assert_eq!(
            ancestor_dirs_to_root(Path::new("/proj/file.rs"), &roots),
            vec![PathBuf::from("/proj")]
        );
    }

    #[test]
    fn ancestor_dirs_empty_when_outside_any_root() {
        let roots = vec![PathBuf::from("/proj")];
        assert!(ancestor_dirs_to_root(Path::new("/other/file.rs"), &roots).is_empty());
    }

    #[test]
    fn ancestor_dirs_empty_when_root_is_a_file() {
        // Degenerate: a file passed as the watched root → no ancestor dirs to enqueue
        // (must not walk up to the filesystem root).
        let roots = vec![PathBuf::from("/proj/solo.txt")];
        assert!(ancestor_dirs_to_root(Path::new("/proj/solo.txt"), &roots).is_empty());
    }
}
