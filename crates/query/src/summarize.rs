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

/// Strip a leading conversational preamble some LLMs emit before the actual summary
/// (e.g. "Here's a refined summary:", "Summary:") and trim.
///
/// The LangChain-style refine prompt ("refine the original summary…") invites such
/// meta-commentary. Running this in the summarize loop guarantees three things the
/// raw `.trim()` did not: the stored **L1** summary is clean; the **L0** abstract
/// `upsert_summary` derives from it is clean; and the loop's "no change" early-stop
/// can actually fire on idempotent passes (a re-wrapped preamble used to make every
/// pass compare unequal, so on `--passes 3` preambles compounded).
///
/// Conservative by design — only strips a leading segment that is clearly a
/// summary/description meta-label, never real content.
pub(crate) fn strip_summary_preamble(s: &str) -> String {
    let mut text = s.trim();
    // A preamble can be re-wrapped across passes; peel a few layers, but bounded.
    for _ in 0..3 {
        match strip_one_preamble(text) {
            Some(rest) if !rest.is_empty() => text = rest.trim(),
            _ => break,
        }
    }
    text.to_owned()
}

/// Strip a single leading meta-preamble segment, returning the remainder, or `None`
/// when the text doesn't begin with one.
fn strip_one_preamble(text: &str) -> Option<&str> {
    // Only the first line can be a "…:" label.
    let first_line_end = text.find('\n').unwrap_or(text.len());
    let colon = text[..first_line_end].find(':')?; // ':' is ASCII → char boundary
    let label = text[..colon].trim().to_ascii_lowercase();
    // Bound the label so a real sentence containing a colon is never eaten.
    if label.is_empty() || label.len() > 60 {
        return None;
    }
    is_preamble_label(&label).then(|| text[colon + 1..].trim_start())
}

/// True ONLY when `label` (lowercased, trimmed) is a pure "here is the summary"-style
/// meta-label — never when it carries substantive content after the core noun.
///
/// Structural, not keyword-loose: peel an optional opener / "here's" / article /
/// adjective, then require what remains to be EXACTLY `summary`/`description` plus
/// meta-filler. So "Updated summary serialization: …" and "Here's how the summary
/// module works: …" are preserved (real content), while "Here's a refined summary:"
/// and a bare "Summary:" are stripped.
fn is_preamble_label(label: &str) -> bool {
    let mut s = label;
    // Optional leading conversational opener — only when followed by space/comma/end
    // (so "okta…" is not mistaken for the "ok" opener).
    for opener in [
        "sure",
        "certainly",
        "okay",
        "ok",
        "of course",
        "alright",
        "right",
    ] {
        if let Some(rest) = s.strip_prefix(opener) {
            if rest.is_empty() || rest.starts_with([' ', ',']) {
                s = rest.trim_start_matches([' ', ',']);
                break;
            }
        }
    }
    // Optional "here's / here is / here are / here".
    for h in ["here's", "here is", "here are", "here"] {
        if let Some(rest) = s.strip_prefix(h) {
            if rest.is_empty() || rest.starts_with(' ') {
                s = rest.trim_start();
                break;
            }
        }
    }
    // Optional article, then optional adjective.
    for a in ["a ", "an ", "the "] {
        if let Some(rest) = s.strip_prefix(a) {
            s = rest;
            break;
        }
    }
    for adj in [
        "refined ",
        "updated ",
        "revised ",
        "concise ",
        "brief ",
        "short ",
        "detailed ",
        "final ",
    ] {
        if let Some(rest) = s.strip_prefix(adj) {
            s = rest;
            break;
        }
    }
    // What remains must be the core noun plus only recognised meta-filler — anything
    // substantive after it (e.g. "summary serialization") means it's real content.
    let Some(rest) = s
        .strip_prefix("summary")
        .or_else(|| s.strip_prefix("description"))
    else {
        return false;
    };
    let rest = rest.trim();
    rest.is_empty()
        || matches!(
            rest,
            "of the file"
                | "of this file"
                | "of the code"
                | "of the directory"
                | "of the folder"
                | "of the contents"
                | "of the module"
                | "for this file"
                | "below"
                | "of it"
        )
}

/// Summarise one file and persist the row. Returns true if successful.
///
/// When `on_fragment` is `Some`, each generated token is forwarded to the
/// callback for live streaming to the web UI.  Pass `None` for the CLI path.
pub async fn summarize_file(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    path: &str,
    model: &str,
    passes: u32,
    mut on_fragment: Option<&mut (dyn FnMut(String) + Send)>,
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

    let mut summary_text: Option<String> = None;
    for i in 0..passes.max(1) {
        let next = match on_fragment {
            Some(ref mut f) => {
                describer
                    .describe_stream(path, &sample, summary_text.as_deref(), *f)
                    .await?
            }
            None => {
                describer
                    .describe(path, &sample, summary_text.as_deref())
                    .await?
            }
        };
        let next = strip_summary_preamble(&next);
        if next.is_empty() {
            break;
        }
        if next == summary_text.as_deref().unwrap_or("") {
            tracing::debug!(
                "summarize {path} pass {}/{passes}: no change, stopping early",
                i + 1
            );
            break;
        }
        tracing::info!("summarize {path} pass {}/{passes}", i + 1);
        summary_text = Some(next);
    }
    let Some(summary_text) = summary_text else {
        return Ok(false);
    };

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
        summary_l0: None, // derived from summary by upsert_summary
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
///
/// When `on_fragment` is `Some`, each generated token is forwarded to the
/// callback for live streaming to the web UI.  Pass `None` for the CLI path.
#[allow(clippy::too_many_arguments)]
pub async fn summarize_directory(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    dir_path: &str,
    dir_model: &str,
    max_children: usize,
    passes: u32,
    mut on_fragment: Option<&mut (dyn FnMut(String) + Send)>,
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

    let mut summary_text: Option<String> = None;
    for i in 0..passes.max(1) {
        let next = match on_fragment {
            Some(ref mut f) => {
                describer
                    .summarize_dir_stream(dir_path, &llm_children, summary_text.as_deref(), *f)
                    .await?
            }
            None => {
                describer
                    .summarize_dir(dir_path, &llm_children, summary_text.as_deref())
                    .await?
            }
        };
        let next = strip_summary_preamble(&next);
        if next.is_empty() {
            break;
        }
        if next == summary_text.as_deref().unwrap_or("") {
            tracing::debug!(
                "summarize {dir_path} pass {}/{passes}: no change, stopping early",
                i + 1
            );
            break;
        }
        tracing::info!("summarize {dir_path} pass {}/{passes}", i + 1);
        summary_text = Some(next);
    }
    let Some(summary_text) = summary_text else {
        return Ok(false);
    };

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
        summary_l0: None, // derived from summary by upsert_summary
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
/// Returns `Ok(true)` on success, `Ok(false)` if summarization failed (the item is
/// recorded as `failed` in the queue), or `Err` only for an unexpected store error.
pub async fn process_queue_item(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    item: &QueueItem,
    cfg: &DescriberConfig,
) -> Result<bool> {
    process_queue_item_with_passes(store, describer, embedder, item, cfg, None, None).await
}

/// Like `process_queue_item` but accepts an explicit pass override and an optional
/// streaming callback for live AI output in the web UI.
///
/// `on_fragment` receives each generated token as it arrives from the LLM.
/// Pass `None` to use non-streaming (CLI path, background worker).
pub async fn process_queue_item_with_passes(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    item: &QueueItem,
    cfg: &DescriberConfig,
    passes_override: Option<u32>,
    on_fragment: Option<&mut (dyn FnMut(String) + Send)>,
) -> Result<bool> {
    let passes = match passes_override {
        Some(n) => n.min(cfg.passes_cap),
        None => {
            let already_summarized = store.summary_by_path(&item.path)?.is_some();
            if already_summarized {
                cfg.passes_refresh
            } else {
                cfg.passes_first
            }
        }
    };

    let result = if item.kind == "file" {
        summarize_file(
            store,
            describer,
            embedder,
            &item.path,
            &cfg.file_model,
            passes,
            on_fragment,
        )
        .await
    } else {
        summarize_directory(
            store,
            describer,
            embedder,
            &item.path,
            &cfg.dir_model,
            cfg.max_children_per_summary,
            passes,
            on_fragment,
        )
        .await
    };

    match result {
        Ok(_) => {
            store.mark_queue_state(&item.path, "done", None)?;
            Ok(true)
        }
        Err(e) => {
            // The item is recorded as `failed` (with the message) and we return Ok(false)
            // rather than Ok(()) so callers can distinguish a real success from a failure
            // — previously every outcome returned Ok(()), so `summarize` reported success
            // even when the model was missing and every item failed.
            let msg = format!("{e:#}");
            tracing::warn!("summarize failed for {}: {msg}", item.path);
            store.mark_queue_state(&item.path, "failed", Some(&msg))?;
            Ok(false)
        }
    }
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
/// `passes_override` overrides the config's automatic first/refresh selection when Some.
pub async fn summarize_subtree_sync(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    root: &Path,
    cfg: &DescriberConfig,
    passes_override: Option<u32>,
) -> Result<usize> {
    let enqueued = enqueue_subtree(store, root)
        .with_context(|| format!("enqueuing subtree {}", root.display()))?;
    println!("Enqueued {enqueued} items for summarization.");

    let mut done = 0usize;
    let mut errors = 0usize;
    let mut first_error: Option<String> = None;
    while let Some(item) = store.next_queue_item()? {
        // CLI path: no streaming callback (None).
        let r = process_queue_item_with_passes(
            store,
            describer,
            embedder,
            &item,
            cfg,
            passes_override,
            None,
        )
        .await;
        match r {
            Ok(true) => done += 1,
            Ok(false) => errors += 1,
            Err(e) => {
                errors += 1;
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
        if (done + errors).is_multiple_of(10) {
            println!(
                "  {}/{enqueued} processed ({errors} errors)...",
                done + errors
            );
        }
    }

    if errors > 0 && done == 0 {
        // Most failures now arrive as Ok(false) (the item is marked `failed` in the queue),
        // so fall back to the queue's recorded error for the guidance message.
        let msg = first_error
            .or_else(|| {
                store
                    .failed_queue_items(1)
                    .ok()
                    .and_then(|v| v.into_iter().next())
                    .and_then(|f| f.error)
            })
            .unwrap_or_default();
        anyhow::bail!(
            "summarize failed: 0/{} items succeeded. First error: {msg}\n\
             Did you run `ollama pull {}`?",
            enqueued,
            cfg.dir_model
        );
    }
    if errors > 0 {
        eprintln!("Warning: summarize completed with {errors} errors.");
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

#[cfg(test)]
mod tests {
    use super::strip_summary_preamble;

    #[test]
    fn strips_here_is_refined_summary() {
        assert_eq!(
            strip_summary_preamble(
                "Here's a refined summary of the file:\n\nA Rust module that parses TOML."
            ),
            "A Rust module that parses TOML."
        );
    }

    #[test]
    fn strips_bare_summary_label() {
        assert_eq!(
            strip_summary_preamble("Summary: Defines the CLI entrypoint."),
            "Defines the CLI entrypoint."
        );
    }

    #[test]
    fn strips_refined_summary_label() {
        assert_eq!(
            strip_summary_preamble("Refined summary: Handles auth."),
            "Handles auth."
        );
    }

    #[test]
    fn strips_here_is_a_brief_description() {
        assert_eq!(
            strip_summary_preamble(
                "Sure, here is a brief description: It rolls up child summaries."
            ),
            "It rolls up child summaries."
        );
    }

    #[test]
    fn keeps_real_content_with_a_colon() {
        // A leading clause with a colon that is NOT a summary meta-label must survive.
        let s = "Here is the build configuration: it compiles with cargo and clippy.";
        assert_eq!(strip_summary_preamble(s), s);
    }

    #[test]
    fn keeps_content_that_merely_mentions_summary() {
        // No colon → nothing to strip; real content preserved verbatim.
        let s = "A summary of recent changes to the parser pipeline lives here.";
        assert_eq!(strip_summary_preamble(s), s);
    }

    #[test]
    fn peels_compounded_preambles() {
        assert_eq!(
            strip_summary_preamble(
                "Here's a refined summary:\n\nHere is a summary:\n\nThe core engine."
            ),
            "The core engine."
        );
    }

    #[test]
    fn plain_summary_is_only_trimmed() {
        assert_eq!(
            strip_summary_preamble("  Just a plain summary sentence.  "),
            "Just a plain summary sentence."
        );
    }

    #[test]
    fn preamble_only_is_kept_not_emptied() {
        // Pathological: model returned nothing but a preamble — keep it rather than
        // store an empty summary.
        let s = "Here's a refined summary:";
        assert_eq!(strip_summary_preamble(s), s);
    }

    #[test]
    fn enables_early_stop_idempotence() {
        // Two passes whose only difference is the preamble compare equal once stripped,
        // so the loop's "no change" early-stop can fire.
        let a = strip_summary_preamble("Here's a summary: The file defines X.");
        let b = strip_summary_preamble("Here is a refined summary: The file defines X.");
        assert_eq!(a, b);
        assert_eq!(a, "The file defines X.");
    }

    // ── False-positive guards: real content whose first clause names "summary" /
    //    "description" before a colon must NEVER be truncated. These are exactly the
    //    summaries Indexa would generate when indexing its own summarization code. ──

    #[test]
    fn keeps_content_describing_a_summary_feature() {
        for s in [
            "Here's how the summary module works: it caches results per path.",
            "Updated summary serialization: now emits ISO timestamps.",
            "Concise summary generator: builds L0 abstracts from L1 text.",
            "Revised description handling: trims whitespace and newlines.",
            "Brief description of each summary tier: L0 is one sentence, L1 is full.",
            "Summary of changes: adds the refine loop and a backstop.",
            "Okta integration summary: wires SSO into the gateway.",
        ] {
            assert_eq!(strip_summary_preamble(s), s, "must not truncate: {s:?}");
        }
    }

    #[test]
    fn compound_does_not_over_strip_real_subject() {
        // Peel the genuine first preamble, but stop before deleting the real subject
        // of the next clause ("Updated description schema").
        assert_eq!(
            strip_summary_preamble(
                "Here's a summary: Updated description schema: now supports nested keys."
            ),
            "Updated description schema: now supports nested keys."
        );
    }

    #[test]
    fn strips_preamble_with_meta_filler_tail() {
        assert_eq!(
            strip_summary_preamble("Sure, here's a summary of the file: It issues JWTs."),
            "It issues JWTs."
        );
    }
}
