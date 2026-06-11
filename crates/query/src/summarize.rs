//! Bottom-up hierarchical summarization algorithm.
//!
//! Phase 1 (file pass): describe each parseable file using a content sample.
//! Phase 2 (directory pass): roll up from deepest directories to root, composing
//! child summaries into a parent summary. Each level is embedded into the same
//! vector space as chunks so they participate in hybrid retrieval.

use anyhow::{Context, Result};
use indexa_core::{
    config::{DescriberConfig, SummaryMode},
    store::{dir_source_hash, file_source_hash, QueueItem, Store, SummaryRecord},
};
use indexa_embed::Embedder;
use indexa_llm::{ChildSummary, Describer};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::MAX_DIR_DEFERS;

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
#[allow(clippy::too_many_arguments)]
pub async fn summarize_file(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    path: &str,
    model: &str,
    passes: u32,
    model_fallback: bool,
    mut on_fragment: Option<&mut (dyn FnMut(String) + Send)>,
) -> Result<SummaryWrite> {
    // Freshness gate (incremental re-summarize): hash the file's current content
    // and skip the LLM entirely when it matches the stored summary's source_hash —
    // the existing summary is still true for these exact bytes, so the row (and its
    // provenance) is left untouched. An empty hash (unreadable file) never matches,
    // so an unhashable file degrades to "always re-summarize", never a stale skip.
    // The skip is a success: the caller's queue item still completes as `done`.
    // Stamped BEFORE reading content: generated_at must lower-bound the bytes
    // the summary describes, so an edit landing during the (seconds-to-minutes)
    // LLM run still reads as modified_s >= generated_at on the next refresh.
    let generated_at = now_secs();
    let source_hash = file_source_hash(Path::new(path));
    if !source_hash.is_empty() {
        // A read error degrades to "not summarized yet" (re-summarize), never a skip.
        if let Ok(Some(existing)) = store.summary_by_path(path) {
            if existing.source_hash == source_hash {
                tracing::debug!("summarize {path}: content unchanged, skipping");
                return Ok(SummaryWrite::Skipped);
            }
        }
    }

    // Try to get a content sample. Prefer first chunk text (already parsed),
    // fall back to raw file bytes.
    let sample: Vec<u8> = if let Ok(Some(first_chunk)) = store.first_chunk_text(path) {
        first_chunk.into_bytes()
    } else {
        match std::fs::read(path) {
            Ok(bytes) => bytes.into_iter().take(4096).collect(),
            Err(_) => return Ok(SummaryWrite::NoContent), // unreadable file
        }
    };

    let mut summary_text: Option<String> = None;
    let mut passes_run: i64 = 0;
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
        passes_run = i64::from(i) + 1;
    }
    let Some(summary_text) = summary_text else {
        return Ok(SummaryWrite::NoContent);
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
        source_hash,
        generated_at,
    })?;
    store.set_summary_provenance(path, describer.provider_name(), passes_run, model_fallback)?;

    Ok(SummaryWrite::Written)
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
    model_fallback: bool,
    mut on_fragment: Option<&mut (dyn FnMut(String) + Send)>,
) -> Result<SummaryWrite> {
    // Start-of-run stamp — same reasoning as summarize_file's.
    let generated_at = now_secs();
    let children = store.children_summaries(dir_path)?;
    if children.is_empty() {
        return Ok(SummaryWrite::NoContent);
    }

    // Freshness gate: Merkle-style roll-up over ALL children's hashes (not the
    // max_children LLM truncation, so membership changes and far-child edits still
    // re-roll). Equal non-empty hash ⇒ the subtree's content is byte-identical to
    // what this roll-up was built from — skip the LLM, keep the row + provenance.
    // Any child with an empty hash (legacy/unreadable) yields "" ⇒ never skip.
    let source_hash = dir_source_hash(&children);
    if !source_hash.is_empty() {
        if let Ok(Some(existing)) = store.summary_by_path(dir_path) {
            if existing.source_hash == source_hash {
                tracing::debug!("summarize {dir_path}: children unchanged, skipping roll-up");
                return Ok(SummaryWrite::Skipped);
            }
        }
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
    let mut passes_run: i64 = 0;
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
        passes_run = i64::from(i) + 1;
    }
    let Some(summary_text) = summary_text else {
        return Ok(SummaryWrite::NoContent);
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
        source_hash,
        generated_at,
    })?;
    store.set_summary_provenance(
        dir_path,
        describer.provider_name(),
        passes_run,
        model_fallback,
    )?;

    Ok(SummaryWrite::Written)
}

/// What one summarize call actually did — distinguishes real LLM work from the
/// freshness gate's no-op, so surfaces can report "N unchanged (skipped)"
/// instead of letting a near-instant refresh look broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryWrite {
    /// A summary row was (re)generated and written.
    Written,
    /// Content hash matched the stored row — no LLM call, row untouched.
    Skipped,
    /// Nothing to summarize (unreadable file / no children).
    NoContent,
}

/// Outcome of processing one summary-queue item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// Summarized and marked `done`.
    Completed,
    /// Content unchanged — the freshness gate skipped the LLM; marked `done`.
    CompletedUnchanged,
    /// Marked `failed` (summarization error).
    Failed,
    /// A directory whose subtree still has unfinished children — re-enqueued `pending`
    /// to roll up once they're done. The caller should back off briefly before the next
    /// claim (a deferred dir stays `pending`, so it is never lost). See
    /// `process_queue_item_with_passes` and the drain loops.
    Deferred,
}

/// Process one item from the summary queue (called by the background worker).
/// `Err` is returned only for an unexpected store error (the caller terminalizes it).
pub async fn process_queue_item(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    item: &QueueItem,
    cfg: &DescriberConfig,
    force_rollup: bool,
) -> Result<QueueOutcome> {
    process_queue_item_with_passes(
        store,
        describer,
        embedder,
        item,
        cfg,
        None,
        None,
        force_rollup,
    )
    .await
}

/// Like `process_queue_item` but accepts an explicit pass override and an optional
/// streaming callback for live AI output in the web UI.
///
/// `on_fragment` receives each generated token as it arrives from the LLM.
/// Pass `None` to use non-streaming (CLI path, background worker).
///
/// `force_rollup` summarizes a directory even when its subtree still has unfinished
/// children. Drain loops set it after repeatedly deferring the same directory (a
/// safety cap so a stuck/hung child can't block a job forever); the strictly-serial
/// `summarize_subtree_sync` sets it always (deepest-first means a dir's children are
/// already terminal when it is claimed).
#[allow(clippy::too_many_arguments)] // all params are load-bearing for this processing entry point
pub async fn process_queue_item_with_passes(
    store: &mut Store,
    describer: &dyn Describer,
    embedder: &dyn Embedder,
    item: &QueueItem,
    cfg: &DescriberConfig,
    passes_override: Option<u32>,
    on_fragment: Option<&mut (dyn FnMut(String) + Send)>,
    force_rollup: bool,
) -> Result<QueueOutcome> {
    // Defer a directory roll-up until its subtree's children are summarized, so a
    // concurrent worker can't roll up an incomplete (empty/stale) summary and mark it
    // `done` — the root cause of the dir-rollup race. The atomic claim is untouched; a
    // deferred dir simply goes back to `pending` and is rolled up on a later claim.
    // A read error here degrades to "ready" (summarize now) rather than deferring forever.
    if item.kind == "dir"
        && !force_rollup
        && store
            .subtree_has_unfinished(&item.path, item.depth)
            .unwrap_or(false)
    {
        // Re-enqueue `pending` and undo the claim's `attempts++` (a defer isn't a real
        // attempt) so repeated defers can't trip `requeue_stale_in_flight` into failing
        // the dir after a crash.
        store.defer_queue_item(&item.path)?;
        return Ok(QueueOutcome::Deferred);
    }

    let passes = match passes_override {
        Some(n) => n.min(cfg.passes_cap),
        None => {
            // Don't `?` here: a read error would propagate while the item is still
            // `in_flight`. Treat an unreadable summary row as "not yet summarized"
            // (→ passes_first); the summarize result below always terminalizes the row.
            let already_summarized = store.summary_by_path(&item.path).ok().flatten().is_some();
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
            cfg.model_fallback,
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
            cfg.model_fallback,
            on_fragment,
        )
        .await
    };

    match result {
        Ok(write) => {
            store.mark_queue_state(&item.path, "done", None)?;
            Ok(match write {
                SummaryWrite::Skipped => QueueOutcome::CompletedUnchanged,
                _ => QueueOutcome::Completed,
            })
        }
        Err(e) => {
            // The item is recorded as `failed` (with the message); callers distinguish a
            // real success from a failure rather than treating every outcome as success.
            let msg = format!("{e:#}");
            tracing::warn!("summarize failed for {}: {msg}", item.path);
            store.mark_queue_state(&item.path, "failed", Some(&msg))?;
            Ok(QueueOutcome::Failed)
        }
    }
}

/// Path's `/`-or-`\`-separator count — the same depth metric the queue sorts on
/// (deepest first), so re-pended ancestors roll up after their children.
fn path_depth(path: &str) -> i64 {
    path.chars().filter(|&c| c == '/' || c == '\\').count() as i64
}

/// Enqueue everything under `root` that needs (re-)summarization. Returns the
/// number of distinct items now pending for this run.
///
/// Two halves:
/// 1. **New paths** — `INSERT OR IGNORE`: anything not yet in the queue.
/// 2. **Dirty propagation** (incremental re-summarize) — `INSERT OR IGNORE` cannot
///    reset a `done` row, so on a refresh a *changed* file (and the ancestor dirs
///    whose roll-ups it staled) would otherwise never re-run. Re-pend entries whose
///    on-disk mtime is newer than their summary (cheap SQL pre-filter; the precise
///    content-hash gate in `summarize_file`/`summarize_directory` makes a
///    touched-but-identical path a no-op), plus the ancestor chains — up to `root` —
///    of every stale or newly discovered path (a new file stales its ancestors'
///    roll-ups too; a deletion bumps the parent dir's mtime, which seeds the same
///    walk). Unchanged paths are never re-pended, so they re-pend no ancestors.
///
/// Use [`requeue_subtree`] when the user explicitly asks to regenerate everything.
pub fn enqueue_subtree(store: &mut Store, root: &Path) -> Result<usize> {
    let root_str = root.to_string_lossy();
    let items = store.entries_for_summarization(&root_str)?;

    let depth_items: Vec<(String, String, i64)> = items
        .into_iter()
        .map(|(path, kind)| {
            let depth = path_depth(&path);
            (path, kind, depth)
        })
        .collect();

    store.enqueue_summary_items(&depth_items)?;

    // Dirty propagation: stale entries + ancestors of stale/new paths.
    let stale = store.stale_summary_candidates(&root_str)?;
    let mut seen: HashSet<String> = depth_items.iter().map(|(p, _, _)| p.clone()).collect();
    let mut dirty: Vec<(String, String, i64)> = Vec::new();
    for (path, kind) in &stale {
        if seen.insert(path.clone()) {
            dirty.push((path.clone(), kind.clone(), path_depth(path)));
        }
    }
    // Ancestor walk for every seed (stale + newly enqueued). On a first build all
    // ancestors are already in `seen` (everything was just enqueued), so this adds
    // nothing; on a refresh it re-pends exactly the chains that changed.
    let seeds: Vec<String> = stale
        .iter()
        .map(|(p, _)| p.clone())
        .chain(depth_items.iter().map(|(p, _, _)| p.clone()))
        .collect();
    for seed in &seeds {
        let mut cur = Path::new(seed).parent();
        while let Some(d) = cur {
            // Stay within the enqueued subtree: ancestors above `root` belong to a
            // different root's roll-up scope (mirrors the watcher's ancestor walk).
            if !d.starts_with(root) {
                break;
            }
            let d_str = d.to_string_lossy().into_owned();
            if seen.insert(d_str.clone()) {
                let depth = path_depth(&d_str);
                dirty.push((d_str, "dir".to_owned(), depth));
            }
            if d == root {
                break;
            }
            cur = d.parent();
        }
    }
    store.mark_for_resummary_batch(&dirty)?;

    Ok(depth_items.len() + dirty.len())
}

/// Force-requeue all files + directories under `root` for (re-)summarization,
/// resetting any existing `done`/`failed` rows back to `pending`.
///
/// Unlike [`enqueue_subtree`] (which uses `INSERT OR IGNORE` and skips existing rows),
/// this function calls `mark_for_resummary` for every entry so that already-summarized
/// paths are re-processed — making "Regenerate" actually regenerate.
///
/// Returns the number of items reset or newly enqueued.
pub fn requeue_subtree(store: &mut Store, root: &Path) -> Result<usize> {
    let root_str = root.to_string_lossy();
    // Explicit regenerate: blank the stored hashes FIRST so the freshness gate
    // can't skip byte-identical content — Regenerate exists precisely for the
    // cases the hash can't see (model switch, prompt change, user judgment).
    store.clear_summary_hashes_under(&root_str)?;
    let items = store.entries_for_resummary(&root_str)?;
    let n = items.len();
    for (path, kind, depth) in &items {
        store.mark_for_resummary(path, kind, *depth)?;
    }
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
) -> Result<(usize, usize)> {
    let enqueued = enqueue_subtree(store, root)
        .with_context(|| format!("enqueuing subtree {}", root.display()))?;
    println!("Enqueued {enqueued} items for summarization.");

    let mut done = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;
    let mut first_error: Option<String> = None;
    // Per-dir defer count for the force-rollup cap. In a solo serial drain this never
    // fills (deepest-first ⇒ a dir's children are terminal when it's claimed), but a
    // worker daemon sharing the DB can hold a child `in_flight`, so we defer (not force)
    // and let the cap backstop a genuinely-stuck child.
    let mut defers: HashMap<String, u32> = HashMap::new();
    while let Some(item) = store.next_queue_item()? {
        let force =
            item.kind == "dir" && defers.get(&item.path).copied().unwrap_or(0) >= MAX_DIR_DEFERS;
        // CLI path: no streaming callback (None).
        let r = process_queue_item_with_passes(
            store,
            describer,
            embedder,
            &item,
            cfg,
            passes_override,
            None,
            force,
        )
        .await;
        match r {
            Ok(QueueOutcome::Completed) => {
                defers.remove(&item.path);
                done += 1;
            }
            Ok(QueueOutcome::CompletedUnchanged) => {
                defers.remove(&item.path);
                skipped += 1;
            }
            Ok(QueueOutcome::Failed) => {
                defers.remove(&item.path);
                errors += 1;
            }
            Ok(QueueOutcome::Deferred) => {
                *defers.entry(item.path.clone()).or_insert(0) += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
            Err(e) => {
                defers.remove(&item.path);
                errors += 1;
                // A store error left the claimed row `in_flight`; terminalize it (best-effort)
                // so it isn't stuck for the rest of this process (this CLI loop runs no startup
                // sweep). Mirrors the worker + web drain loops.
                if let Err(mark_err) =
                    store.mark_queue_state(&item.path, "failed", Some(&format!("{e:#}")))
                {
                    tracing::warn!(
                        path = %item.path,
                        error = %mark_err,
                        "summarize: failed to terminalize stuck queue row as failed; it may stay in_flight"
                    );
                }
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
        if (done + skipped + errors).is_multiple_of(10) {
            println!(
                "  {}/{enqueued} processed ({errors} errors)...",
                done + skipped + errors
            );
        }
    }

    if errors > 0 && done == 0 && skipped == 0 {
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
    Ok((done, skipped))
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

#[cfg(test)]
mod scheduler_tests {
    use super::{process_queue_item_with_passes, QueueOutcome};
    use indexa_core::config::DescriberConfig;
    use indexa_core::store::{QueueItem, Store, SummaryRecord};
    use indexa_embed::Embedder;
    use indexa_llm::{ChildSummary, Describer};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct StubEmbedder;
    #[async_trait::async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, _t: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }

    /// Describer that counts directory roll-up calls, so a test can assert the roll-up
    /// was (not) attempted.
    struct CountingDescriber {
        dir_calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Describer for CountingDescriber {
        async fn describe(
            &self,
            _p: &str,
            _c: &[u8],
            _prev: Option<&str>,
        ) -> anyhow::Result<String> {
            Ok("file summary".into())
        }
        async fn summarize_dir(
            &self,
            _d: &str,
            _children: &[ChildSummary],
            _prev: Option<&str>,
        ) -> anyhow::Result<String> {
            self.dir_calls.fetch_add(1, Ordering::SeqCst);
            Ok("dir summary".into())
        }
    }

    fn dir_item() -> QueueItem {
        QueueItem {
            path: "/d".into(),
            kind: "dir".into(),
            depth: 1,
        }
    }

    #[tokio::test]
    async fn dir_defers_while_a_child_is_pending() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .enqueue_summary_items(&[
                ("/d".into(), "dir".into(), 1),
                ("/d/f.txt".into(), "file".into(), 2),
            ])
            .unwrap();
        let dir_calls = Arc::new(AtomicUsize::new(0));
        let dsc = CountingDescriber {
            dir_calls: dir_calls.clone(),
        };
        let out = process_queue_item_with_passes(
            &mut store,
            &dsc,
            &StubEmbedder,
            &dir_item(),
            &DescriberConfig::default(),
            None,
            None,
            false, // not forced
        )
        .await
        .unwrap();
        assert_eq!(out, QueueOutcome::Deferred);
        assert_eq!(
            dir_calls.load(Ordering::SeqCst),
            0,
            "the roll-up must NOT run while a child is still pending"
        );
        // The dir was left/returned `pending`, not marked `done`.
        assert_eq!(store.queue_stats().unwrap().done, 0);
    }

    #[tokio::test]
    async fn force_rollup_summarizes_despite_a_pending_child() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .enqueue_summary_items(&[
                ("/d".into(), "dir".into(), 1),
                ("/d/f.txt".into(), "file".into(), 2),
            ])
            .unwrap();
        // A child summary exists to roll up (the pending queue row remains, simulating a
        // stuck sibling); force_rollup must summarize anyway and mark the dir done.
        store
            .upsert_summary(&SummaryRecord {
                path: "/d/f.txt".into(),
                kind: "file".into(),
                parent_path: Some("/d".into()),
                depth: 2,
                summary: "child summary".into(),
                summary_l0: None,
                embedding: None,
                child_count: 0,
                byte_size: 0,
                model: "stub".into(),
                source_hash: String::new(),
                generated_at: 0,
            })
            .unwrap();
        let dir_calls = Arc::new(AtomicUsize::new(0));
        let dsc = CountingDescriber {
            dir_calls: dir_calls.clone(),
        };
        let out = process_queue_item_with_passes(
            &mut store,
            &dsc,
            &StubEmbedder,
            &dir_item(),
            &DescriberConfig::default(),
            None,
            None,
            true, // forced
        )
        .await
        .unwrap();
        assert_eq!(out, QueueOutcome::Completed);
        assert!(
            dir_calls.load(Ordering::SeqCst) >= 1,
            "force_rollup summarizes the dir despite the pending child"
        );
    }
}

#[cfg(test)]
mod incremental_tests {
    //! Incremental re-summarize: the content-hash skip gates and the dirty
    //! propagation in `enqueue_subtree`. The describers count calls so a test can
    //! assert the LLM was (not) re-paid.

    use super::{
        enqueue_subtree, process_queue_item_with_passes, summarize_directory, summarize_file,
        QueueOutcome, SummaryWrite,
    };
    use indexa_core::config::DescriberConfig;
    use indexa_core::store::{Store, SummaryRecord};
    use indexa_core::walker::{Entry, EntryKind};
    use indexa_embed::Embedder;
    use indexa_llm::{ChildSummary, Describer};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct StubEmbedder;
    #[async_trait::async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, _t: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }

    /// Counts file describes and dir roll-ups separately, so a skip can be proven
    /// as "zero calls", not just "no row change".
    struct CountingDescriber {
        file_calls: Arc<AtomicUsize>,
        dir_calls: Arc<AtomicUsize>,
    }
    impl CountingDescriber {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let f = Arc::new(AtomicUsize::new(0));
            let d = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    file_calls: f.clone(),
                    dir_calls: d.clone(),
                },
                f,
                d,
            )
        }
    }
    #[async_trait::async_trait]
    impl Describer for CountingDescriber {
        async fn describe(
            &self,
            _p: &str,
            _c: &[u8],
            _prev: Option<&str>,
        ) -> anyhow::Result<String> {
            self.file_calls.fetch_add(1, Ordering::SeqCst);
            Ok("file summary".into())
        }
        async fn summarize_dir(
            &self,
            _d: &str,
            _children: &[ChildSummary],
            _prev: Option<&str>,
        ) -> anyhow::Result<String> {
            self.dir_calls.fetch_add(1, Ordering::SeqCst);
            Ok("dir summary".into())
        }
    }

    fn child_summary(path: &str, parent: &str, hash: &str) -> SummaryRecord {
        SummaryRecord {
            path: path.to_owned(),
            kind: "file".into(),
            parent_path: Some(parent.to_owned()),
            depth: super::path_depth(path),
            summary: format!("summary of {path}"),
            summary_l0: None,
            embedding: None,
            child_count: 0,
            byte_size: 1,
            model: "stub".into(),
            source_hash: hash.to_owned(),
            generated_at: 1,
        }
    }

    // ── File skip gate ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn unchanged_file_skips_the_describer_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "stable content").unwrap();
        let path = f.to_string_lossy().into_owned();
        let mut store = Store::open_in_memory().unwrap();
        let (dsc, file_calls, _) = CountingDescriber::new();

        let first = summarize_file(&mut store, &dsc, &StubEmbedder, &path, "m", 1, false, None)
            .await
            .unwrap();
        assert_eq!(first, SummaryWrite::Written);
        assert_eq!(file_calls.load(Ordering::SeqCst), 1);
        let row = store.summary_by_path(&path).unwrap().unwrap();
        assert!(
            !row.source_hash.is_empty(),
            "summarize_file must persist the content hash"
        );

        let second = summarize_file(&mut store, &dsc, &StubEmbedder, &path, "m", 1, false, None)
            .await
            .unwrap();
        assert_eq!(
            second,
            SummaryWrite::Skipped,
            "the skip is a no-op success, not a failure"
        );
        assert_eq!(
            file_calls.load(Ordering::SeqCst),
            1,
            "an unchanged file must not re-pay the LLM"
        );
    }

    #[tokio::test]
    async fn changed_file_resummarizes_and_updates_the_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "version one").unwrap();
        let path = f.to_string_lossy().into_owned();
        let mut store = Store::open_in_memory().unwrap();
        let (dsc, file_calls, _) = CountingDescriber::new();

        summarize_file(&mut store, &dsc, &StubEmbedder, &path, "m", 1, false, None)
            .await
            .unwrap();
        let hash1 = store.summary_by_path(&path).unwrap().unwrap().source_hash;

        std::fs::write(&f, "version two — different bytes").unwrap();
        summarize_file(&mut store, &dsc, &StubEmbedder, &path, "m", 1, false, None)
            .await
            .unwrap();
        let hash2 = store.summary_by_path(&path).unwrap().unwrap().source_hash;

        assert_eq!(
            file_calls.load(Ordering::SeqCst),
            2,
            "changed content re-runs"
        );
        assert_ne!(hash1, hash2, "the stored hash must track the new content");
        assert!(!hash2.is_empty());
    }

    // ── Dir skip gate (Merkle roll-up) ───────────────────────────────────────

    #[tokio::test]
    async fn dir_skips_when_children_unchanged() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&child_summary("/d/a.txt", "/d", "h1"))
            .unwrap();
        store
            .upsert_summary(&child_summary("/d/b.txt", "/d", "h2"))
            .unwrap();
        let (dsc, _, dir_calls) = CountingDescriber::new();

        summarize_directory(
            &mut store,
            &dsc,
            &StubEmbedder,
            "/d",
            "m",
            10,
            1,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(dir_calls.load(Ordering::SeqCst), 1);
        let row = store.summary_by_path("/d").unwrap().unwrap();
        assert!(
            !row.source_hash.is_empty(),
            "dir roll-up hash must be stored"
        );

        let again = summarize_directory(
            &mut store,
            &dsc,
            &StubEmbedder,
            "/d",
            "m",
            10,
            1,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(again, SummaryWrite::Skipped, "the skip is a no-op success");
        assert_eq!(
            dir_calls.load(Ordering::SeqCst),
            1,
            "unchanged children must not re-pay the roll-up"
        );
    }

    #[tokio::test]
    async fn dir_rerolls_when_a_child_hash_changed() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&child_summary("/d/a.txt", "/d", "h1"))
            .unwrap();
        store
            .upsert_summary(&child_summary("/d/b.txt", "/d", "h2"))
            .unwrap();
        let (dsc, _, dir_calls) = CountingDescriber::new();

        summarize_directory(
            &mut store,
            &dsc,
            &StubEmbedder,
            "/d",
            "m",
            10,
            1,
            false,
            None,
        )
        .await
        .unwrap();
        // A child was re-summarized from new content → its hash moved.
        store
            .upsert_summary(&child_summary("/d/a.txt", "/d", "h1-changed"))
            .unwrap();
        summarize_directory(
            &mut store,
            &dsc,
            &StubEmbedder,
            "/d",
            "m",
            10,
            1,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            dir_calls.load(Ordering::SeqCst),
            2,
            "a changed child must re-roll the dir"
        );
    }

    #[tokio::test]
    async fn dir_never_skips_on_a_legacy_empty_child_hash() {
        // A child with hash "" (legacy row / unreadable file) means the dir's
        // freshness is unknown — it must re-roll every time rather than lie.
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&child_summary("/d/a.txt", "/d", "h1"))
            .unwrap();
        store
            .upsert_summary(&child_summary("/d/b.txt", "/d", ""))
            .unwrap();
        let (dsc, _, dir_calls) = CountingDescriber::new();

        for _ in 0..2 {
            summarize_directory(
                &mut store,
                &dsc,
                &StubEmbedder,
                "/d",
                "m",
                10,
                1,
                false,
                None,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            dir_calls.load(Ordering::SeqCst),
            2,
            "an unhashable child disables the skip, never enables a stale one"
        );
    }

    // ── Queue state machine: a skip still completes ──────────────────────────

    #[tokio::test]
    async fn skipped_file_still_completes_for_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "stable content").unwrap();
        let path = f.to_string_lossy().into_owned();
        let mut store = Store::open_in_memory().unwrap();
        store
            .enqueue_summary_items(&[(path.clone(), "file".into(), super::path_depth(&path))])
            .unwrap();
        let (dsc, file_calls, _) = CountingDescriber::new();
        let cfg = DescriberConfig::default();

        // First drain: summarizes for real (passes pinned to 1 for a stable count).
        let item = store.next_queue_item().unwrap().unwrap();
        let out = process_queue_item_with_passes(
            &mut store,
            &dsc,
            &StubEmbedder,
            &item,
            &cfg,
            Some(1),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(out, QueueOutcome::Completed);
        assert_eq!(file_calls.load(Ordering::SeqCst), 1);

        // Re-pend (as watch / a refresh would) and drain again: content unchanged →
        // the skip must still terminalize the row as `done`, or the queue would
        // claim it forever.
        store
            .mark_for_resummary(&path, "file", super::path_depth(&path))
            .unwrap();
        let item = store.next_queue_item().unwrap().unwrap();
        let out = process_queue_item_with_passes(
            &mut store,
            &dsc,
            &StubEmbedder,
            &item,
            &cfg,
            Some(1),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            QueueOutcome::CompletedUnchanged,
            "a skip completes the queue item, distinguishably"
        );
        assert_eq!(
            file_calls.load(Ordering::SeqCst),
            1,
            "the skip must not call the describer"
        );
        assert_eq!(store.queue_state(&path).unwrap().as_deref(), Some("done"));
    }

    // ── Dirty propagation in enqueue_subtree ─────────────────────────────────

    fn entry(path: &str, kind: EntryKind, mtime_secs: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size: 1,
            modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs)),
            hint: None,
        }
    }

    fn done_with_summary(store: &mut Store, path: &str, kind: &str, generated_at: i64) {
        store.mark_queue_state(path, "done", None).unwrap();
        store
            .upsert_summary(&SummaryRecord {
                path: path.to_owned(),
                kind: kind.to_owned(),
                parent_path: Path::new(path)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned()),
                depth: super::path_depth(path),
                summary: "s".into(),
                summary_l0: None,
                embedding: None,
                child_count: 0,
                byte_size: 1,
                model: "stub".into(),
                source_hash: "h".into(),
                generated_at,
            })
            .unwrap();
    }

    #[test]
    fn enqueue_subtree_repends_changed_files_and_their_ancestors_only() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                entry("/r", EntryKind::Dir, 100),
                entry("/r/a", EntryKind::Dir, 100),
                entry("/r/a/f1", EntryKind::File, 100),
                entry("/r/b", EntryKind::Dir, 100),
                entry("/r/b/f2", EntryKind::File, 100),
            ])
            .unwrap();

        // First build: everything is newly enqueued.
        let n = enqueue_subtree(&mut store, Path::new("/r")).unwrap();
        assert_eq!(n, 5);

        // Simulate a completed drain. f1's summary predates its mtime (stale);
        // every other summary postdates it (fresh).
        done_with_summary(&mut store, "/r", "dir", 200);
        done_with_summary(&mut store, "/r/a", "dir", 200);
        done_with_summary(&mut store, "/r/a/f1", "file", 50);
        done_with_summary(&mut store, "/r/b", "dir", 200);
        done_with_summary(&mut store, "/r/b/f2", "file", 200);

        // Refresh: exactly the stale file + its ancestor chain up to the root.
        let n2 = enqueue_subtree(&mut store, Path::new("/r")).unwrap();
        assert_eq!(n2, 3, "stale file + its two ancestor dirs");
        assert_eq!(
            store.queue_state("/r/a/f1").unwrap().as_deref(),
            Some("pending")
        );
        assert_eq!(
            store.queue_state("/r/a").unwrap().as_deref(),
            Some("pending")
        );
        assert_eq!(store.queue_state("/r").unwrap().as_deref(), Some("pending"));
        // The unchanged branch is untouched — unchanged files re-pend no ancestors.
        assert_eq!(
            store.queue_state("/r/b/f2").unwrap().as_deref(),
            Some("done")
        );
        assert_eq!(store.queue_state("/r/b").unwrap().as_deref(), Some("done"));
    }

    #[test]
    fn enqueue_subtree_repends_new_files_ancestors() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                entry("/r", EntryKind::Dir, 100),
                entry("/r/a", EntryKind::Dir, 100),
                entry("/r/a/f1", EntryKind::File, 100),
            ])
            .unwrap();
        enqueue_subtree(&mut store, Path::new("/r")).unwrap();
        done_with_summary(&mut store, "/r", "dir", 200);
        done_with_summary(&mut store, "/r/a", "dir", 200);
        done_with_summary(&mut store, "/r/a/f1", "file", 200);

        // A new file appears in the summarized tree (rescan stored its entry).
        // Its `done` ancestors' roll-ups are now stale and must re-pend.
        store
            .upsert_entries(&[entry("/r/a/f2", EntryKind::File, 150)])
            .unwrap();
        let n = enqueue_subtree(&mut store, Path::new("/r")).unwrap();
        assert_eq!(n, 3, "new file + its two ancestor dirs");
        assert_eq!(
            store.queue_state("/r/a/f2").unwrap().as_deref(),
            Some("pending")
        );
        assert_eq!(
            store.queue_state("/r/a").unwrap().as_deref(),
            Some("pending")
        );
        assert_eq!(store.queue_state("/r").unwrap().as_deref(), Some("pending"));
        // The pre-existing unchanged file stays done.
        assert_eq!(
            store.queue_state("/r/a/f1").unwrap().as_deref(),
            Some("done")
        );
    }

    #[test]
    fn enqueue_subtree_repends_dir_whose_mtime_bumped() {
        // A deletion/rename inside a dir bumps the dir's mtime without any
        // surviving file changing — the dir (and its ancestors) must re-roll.
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                entry("/r", EntryKind::Dir, 100),
                entry("/r/a", EntryKind::Dir, 100),
                entry("/r/a/f1", EntryKind::File, 100),
            ])
            .unwrap();
        enqueue_subtree(&mut store, Path::new("/r")).unwrap();
        done_with_summary(&mut store, "/r", "dir", 200);
        done_with_summary(&mut store, "/r/a", "dir", 200);
        done_with_summary(&mut store, "/r/a/f1", "file", 200);

        // Rescan after a deletion in /r/a: only the dir's mtime moved.
        store
            .upsert_entries(&[entry("/r/a", EntryKind::Dir, 300)])
            .unwrap();
        let n = enqueue_subtree(&mut store, Path::new("/r")).unwrap();
        assert_eq!(n, 2, "the bumped dir + its ancestor");
        assert_eq!(
            store.queue_state("/r/a").unwrap().as_deref(),
            Some("pending")
        );
        assert_eq!(store.queue_state("/r").unwrap().as_deref(), Some("pending"));
        assert_eq!(
            store.queue_state("/r/a/f1").unwrap().as_deref(),
            Some("done")
        );
    }
}
