//! Bottom-up hierarchical summarization algorithm.
//!
//! Phase 1 (file pass): describe each parseable file using a content sample.
//! Phase 2 (directory pass): roll up from deepest directories to root, composing
//! child summaries into a parent summary. Each level is embedded into the same
//! vector space as chunks so they participate in hybrid retrieval.

use anyhow::{Context, Result};
use indexa_core::{
    config::{DescriberConfig, SummaryMode},
    store::{QueueItem, Store, SummaryRecord},
};
use indexa_embed::Embedder;
use indexa_llm::{ChildSummary, Describer};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Summarise one file and persist the row. Returns true if successful.
pub async fn summarize_file(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    path: &str,
    model: &str,
) -> Result<bool> {
    // Try to get a content sample. Prefer first chunk text (already parsed),
    // fall back to raw file bytes.
    let sample: Vec<u8> = if let Ok(Some(first_chunk)) = store.first_chunk_text(path) {
        first_chunk.into_bytes()
    } else {
        match std::fs::read(path) {
            Ok(bytes) => bytes.into_iter().take(4096).collect(),
            Err(_) => return Ok(false), // skip unreadable files
        }
    };

    let summary_text = describer.describe(path, &sample).await?;
    let summary_text = summary_text.trim().to_owned();
    if summary_text.is_empty() {
        return Ok(false);
    }

    let embedding = embedder.embed(&summary_text).await.ok();

    let depth = path.chars().filter(|&c| c == '/' || c == '\\').count() as i64;
    let parent_path = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned());
    let byte_size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);

    store.upsert_summary(&SummaryRecord {
        path: path.to_owned(),
        kind: "file".into(),
        parent_path,
        depth,
        summary: summary_text,
        embedding,
        child_count: 0,
        byte_size,
        model: model.to_owned(),
        source_hash: String::new(),
        generated_at: now_secs(),
    })?;

    Ok(true)
}

/// Summarise a directory by composing its children's summaries.
pub async fn summarize_directory(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    dir_path: &str,
    dir_model: &str,
    max_children: usize,
) -> Result<bool> {
    let children = store.children_summaries(dir_path)?;
    if children.is_empty() {
        return Ok(false);
    }

    let truncated: Vec<&SummaryRecord> = children.iter().take(max_children).collect();
    let llm_children: Vec<ChildSummary> = truncated
        .iter()
        .map(|c| ChildSummary {
            name: Path::new(&c.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.path.clone()),
            kind: c.kind.clone(),
            summary: c.summary.clone(),
        })
        .collect();

    let summary_text = describer.summarize_dir(dir_path, &llm_children).await?;
    let summary_text = summary_text.trim().to_owned();
    if summary_text.is_empty() {
        return Ok(false);
    }

    let embedding = embedder.embed(&summary_text).await.ok();

    let depth = dir_path.chars().filter(|&c| c == '/' || c == '\\').count() as i64;
    let parent_path = Path::new(dir_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned());
    let byte_size: i64 = children.iter().map(|c| c.byte_size).sum();
    let child_count = children.len() as i64;

    store.upsert_summary(&SummaryRecord {
        path: dir_path.to_owned(),
        kind: "dir".into(),
        parent_path,
        depth,
        summary: summary_text,
        embedding,
        child_count,
        byte_size,
        model: dir_model.to_owned(),
        source_hash: String::new(),
        generated_at: now_secs(),
    })?;

    Ok(true)
}

/// Process one item from the summary queue (called by the background worker).
pub async fn process_queue_item(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    item: &QueueItem,
    cfg: &DescriberConfig,
) -> Result<()> {
    let result = if item.kind == "file" {
        summarize_file(store, describer, embedder, &item.path, &cfg.file_model).await
    } else {
        summarize_directory(
            store,
            describer,
            embedder,
            &item.path,
            &cfg.dir_model,
            cfg.max_children_per_summary,
        )
        .await
    };

    match result {
        Ok(_) => store.mark_queue_state(&item.path, "done", None)?,
        Err(e) => {
            let msg = e.to_string();
            tracing::warn!("summarize failed for {}: {msg}", item.path);
            store.mark_queue_state(&item.path, "failed", Some(&msg))?;
        }
    }
    Ok(())
}

/// Enqueue all files + directories under `root` that are not yet in the queue.
/// Returns the number of items enqueued.
pub fn enqueue_subtree(store: &mut Store, root: &Path) -> Result<usize> {
    let root_str = root.to_string_lossy();
    let items = store.entries_for_summarization(&root_str)?;

    let depth_items: Vec<(String, String, i64)> = items
        .into_iter()
        .map(|(path, kind)| {
            let depth = path.chars().filter(|&c| c == '/' || c == '\\').count() as i64;
            (path, kind, depth)
        })
        .collect();

    let n = depth_items.len();
    store.enqueue_summary_items(&depth_items)?;
    Ok(n)
}

/// Synchronously summarise an entire subtree (no background queue).
/// Progress is printed to stdout. Returns the count of successful summaries.
pub async fn summarize_subtree_sync(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    root: &Path,
    cfg: &DescriberConfig,
) -> Result<usize> {
    let enqueued = enqueue_subtree(store, root)
        .with_context(|| format!("enqueuing subtree {}", root.display()))?;
    println!("Enqueued {enqueued} items for summarization.");

    let mut done = 0usize;
    loop {
        let item = store.next_queue_item()?;
        match item {
            None => break,
            Some(item) => {
                process_queue_item(store, describer, embedder, &item, cfg).await?;
                done += 1;
                if done.is_multiple_of(10) {
                    println!("  {done}/{enqueued} summarized...");
                }
            }
        }
    }

    if cfg.mode == SummaryMode::Compress {
        // Drop chunks for this subtree after summarization
        let root_str = root.to_string_lossy().into_owned();
        store.delete_chunks_for_subtree(&root_str)?;
        println!("Compress mode: chunks removed for {}", root.display());
    }

    println!("Done. {done} summaries generated.");
    Ok(done)
}
