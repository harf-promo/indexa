//! Background job execution bodies — the long `run_*` scan/deep/summarize logic
//! plus their finalization and watchdog helpers. Spawned by the `api_job_*`
//! handlers; no axum routing lives here.

use crate::jobs::{broadcast_only, push, JobEvent, JobHandle, JobStatus, Jobs};
use crate::AppState;
use indexa_core::{
    resource::WatchdogState,
    walker::{walk, WalkConfig},
};
use indexa_query::{process_queue_item_with_passes, requeue_subtree, QueueOutcome, MAX_DIR_DEFERS};
use std::collections::HashMap;
use std::sync::Arc;

mod deep;
mod watchdog;

// The deep phase + memory watchdog live in submodules (v0.61 split). Re-export the
// externally-called `run_deep_phase_standalone` so `crate::jobs_exec::` callers are unchanged;
// the rest are used internally by the orchestrator / summarize phase here.
use deep::run_deep_phase;
pub(crate) use deep::run_deep_phase_standalone;
use watchdog::run_watchdog_check;

/// Sliding-window throughput (items/sec) and ETA (sec) from a window of `(time, cumulative)`
/// progress samples. The caller owns the window (trim to the last few seconds + push the latest
/// sample) and passes the latest cumulative `current` plus the `total` target. Both outputs are
/// `None` until there are ≥2 samples (no rate from a single point). `total.saturating_sub(current)`
/// guards a `current` that briefly exceeds `total` (a pending count can drift as items go
/// in-flight); when `current <= total` it's an ordinary subtraction. Shared by the deep + summarize
/// phases, which computed this identically.
pub(crate) fn throughput_eta(
    samples: &std::collections::VecDeque<(std::time::Instant, u64)>,
    current: u64,
    total: u64,
) -> (Option<f64>, Option<f64>) {
    if samples.len() < 2 {
        return (None, None);
    }
    let (oldest_t, oldest_done) = samples.front().unwrap();
    let elapsed = oldest_t.elapsed().as_secs_f64();
    let rate = if elapsed > 0.0 {
        (current - oldest_done) as f64 / elapsed
    } else {
        0.0
    };
    let eta = if rate > 0.0 {
        total.saturating_sub(current) as f64 / rate
    } else {
        0.0
    };
    (Some(rate), Some(eta))
}

// ── Job runner ────────────────────────────────────────────────────────────────

/// Schedule removal of a job from the registry after 60 s. Allows refreshed
/// clients to re-subscribe to recently-finished jobs and replay history.
pub(crate) fn schedule_cleanup(jobs: Jobs, id: uuid::Uuid) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        jobs.write().await.remove(&id);
    });
}

pub(crate) fn finalize_failed(handle: &Arc<JobHandle>, stage: &str, err: &anyhow::Error) {
    let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
    let error = format!("{err:#}");
    push(
        handle,
        JobEvent::Failed {
            error,
            stage: Some(stage.to_owned()),
            item_path: None,
            chain: if chain.len() > 1 { Some(chain) } else { None },
            code: None,
        },
    );
    handle.set_status(JobStatus::Failed);
}

pub(crate) fn finalize_done(handle: &Arc<JobHandle>, summary: &str) {
    push(
        handle,
        JobEvent::Done {
            summary: summary.to_owned(),
        },
    );
    handle.set_status(JobStatus::Done);
}

/// Emit a terminal Done event noting the job was cancelled mid-run.
pub(crate) fn finalize_cancelled(handle: &Arc<JobHandle>, done: usize) {
    push(
        handle,
        JobEvent::Done {
            summary: format!("Cancelled after {done} items"),
        },
    );
    handle.set_status(JobStatus::Done);
}

/// Walk a path in a blocking thread; on failure, push the error to the job and return None.
/// Acquires a permit from `sem` to limit concurrent walks and prevent rayon pool starvation.
/// Build a `WalkConfig` from `[scan]` config so web jobs walk the SAME file set the CLI does —
/// honoring `respect_gitignore`, `ignore` patterns, `include_sensitive`, and `skip_binary`.
/// (Previously web jobs used `WalkConfig::default()`, silently bypassing user exclusions and
/// causing cross-surface reconcile churn.)
pub(crate) fn scan_walk_config(scan: &indexa_core::config::ScanConfig) -> WalkConfig {
    WalkConfig {
        respect_gitignore: scan.respect_gitignore,
        ignore: scan.ignore.clone(),
        include_sensitive: scan.include_sensitive,
        sniff_binary: scan.skip_binary,
        ..WalkConfig::default()
    }
}

pub(crate) async fn walk_for_job(
    path: &str,
    handle: &Arc<JobHandle>,
    sem: &tokio::sync::Semaphore,
    walk_cfg: WalkConfig,
) -> Option<Vec<indexa_core::walker::Entry>> {
    let _permit = sem.acquire().await.ok()?;
    let pb = std::path::PathBuf::from(path);
    let walked = tokio::task::spawn_blocking(move || walk(&pb, &walk_cfg))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!(e)));
    match walked {
        Ok(e) => Some(e),
        Err(e) => {
            finalize_failed(handle, "walk", &e);
            None
        }
    }
}

pub(crate) async fn run_index_job(
    state: AppState,
    path: String,
    handle: Arc<JobHandle>,
    model_override: Option<(String, String, u32)>,
) {
    // Phase 1: scan
    let Some(entries) = walk_for_job(
        &path,
        &handle,
        &state.walk_semaphore,
        scan_walk_config(&state.config.scan),
    )
    .await
    else {
        return;
    };

    if !run_scan_phase_with_entries(&state, &path, &entries, &handle).await {
        return;
    }
    // Cancellation requested during/after scan — stop before the expensive phases.
    if handle.is_cancelled() {
        finalize_cancelled(&handle, 0);
        return;
    }

    // Phase 2: deep index (its own loop also honors cancellation and emits the
    // terminal event, in which case it returns false and we just stop here).
    if !run_deep_phase(&state, &path, &entries, &handle).await {
        return;
    }
    if handle.is_cancelled() {
        finalize_cancelled(&handle, 0);
        return;
    }

    // Phase 3: summarize
    run_summarize_phase(&state, &path, None, &handle, model_override).await;
}

/// Standalone scan: walks, scans, then finalises the job as done.
pub(crate) async fn run_scan_phase_standalone(
    state: &AppState,
    path: &str,
    handle: &Arc<JobHandle>,
) {
    let Some(entries) = walk_for_job(
        path,
        handle,
        &state.walk_semaphore,
        scan_walk_config(&state.config.scan),
    )
    .await
    else {
        return;
    };
    if run_scan_phase_with_entries(state, path, &entries, handle).await {
        let n = entries.len() as u64;
        finalize_done(handle, &format!("{n} entries scanned"));
    }
}

async fn run_scan_phase_with_entries(
    state: &AppState,
    path: &str,
    entries: &[indexa_core::walker::Entry],
    handle: &Arc<JobHandle>,
) -> bool {
    let n = entries.len() as u64;
    push(
        handle,
        JobEvent::Start {
            kind: "scan".into(),
            path: path.to_owned(),
            total: Some(n),
        },
    );

    let live_paths: std::collections::HashSet<String> = entries
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();

    let mut store = state.store.lock().await;
    if let Err(e) = store.upsert_entries(entries) {
        finalize_failed(handle, "scan", &e);
        return false;
    }
    if let Err(e) = store.reconcile_entries(path, &live_paths) {
        push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("{e:#}"),
                pressure: None,
            },
        );
    }
    // Self-heal: drop chunks/summaries left orphaned (no entry row) — e.g. build artifacts
    // indexed by an older version. `reconcile_entries` only cleans ghost *entries*, not
    // orphans, so without this the index can carry stale junk until a manual `indexa prune`.
    match store.prune_orphans() {
        Ok(orphans) if !orphans.is_empty() => push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!(
                    "pruned {} orphaned chunk(s) and {} summary(ies)",
                    orphans.chunks, orphans.summaries
                ),
                pressure: None,
            },
        ),
        Ok(_) => {}
        Err(e) => push(
            handle,
            JobEvent::Warning {
                stage: "scan".to_owned(),
                item_path: None,
                message: format!("orphan prune skipped: {e:#}"),
                pressure: None,
            },
        ),
    }
    drop(store);

    push(
        handle,
        JobEvent::Progress {
            current: n,
            total: n,
            note: Some(format!("{n} entries scanned")),
            current_path: None,
            items_per_sec: None,
            eta_secs: None,
        },
    );
    true
}

pub(crate) async fn run_summarize_phase(
    state: &AppState,
    path: &str,
    passes_override: Option<u32>,
    handle: &Arc<JobHandle>,
    // Optional (file_model, dir_model, num_ctx) from the "ask me first" popover;
    // when None, the configured describer models are used.
    model_override: Option<(String, String, u32)>,
) {
    push(
        handle,
        JobEvent::Start {
            kind: "summarize".into(),
            path: path.to_owned(),
            total: None,
        },
    );

    let db_path = (*state.db_path).clone();
    let mut cfg = state.config.describer.clone();
    let resource_cfg = state.config.resource.clone();
    let spec = state.machine_spec.clone();
    let headroom = resource_cfg.effective_headroom_bytes();
    let embedder = state.embedder.clone();
    let root = std::path::PathBuf::from(path);
    let (file_model, dir_model, num_ctx) = match &model_override {
        Some((f, d, n)) => (f.clone(), d.clone(), *n),
        None => (cfg.file_model.clone(), cfg.dir_model.clone(), cfg.num_ctx),
    };
    // Keep cfg truthful under an "ask me first" override: summary rows record
    // cfg.file_model/dir_model as their `model`, so a substituted model must be
    // reflected there too; model_fallback marks the substitution in provenance.
    cfg.model_fallback = file_model != cfg.file_model || dir_model != cfg.dir_model;
    cfg.file_model = file_model.clone();
    cfg.dir_model = dir_model.clone();
    cfg.num_ctx = num_ctx;
    // Route through the factory so `provider = "claude-code"` (the user's Claude
    // subscription) is honored here too, not just on the CLI summarize path.
    let describer = match indexa_llm::describer_from_config(
        &cfg.provider,
        &file_model,
        &dir_model,
        &cfg.base_url,
        num_ctx,
        &cfg.claude_bin,
    ) {
        Ok(d) => d,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // Memory watchdog: checked before each LLM summarization call.
    let mut wdog = WatchdogState::new();

    // Open a dedicated Store connection so we can hold it across async LLM awaits
    // without poisoning the shared mutex-wrapped store used by API handlers.
    let mut job_store = match indexa_core::store::Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // Force-requeue the whole subtree: reset any existing `done`/`failed` rows back
    // to `pending` so Regenerate actually re-runs the AI, not just drains new items.
    // `mark_for_resummary` (used internally) leaves `in_flight` rows untouched so
    // concurrent workers aren't double-claimed.
    let newly_enqueued = match requeue_subtree(&mut job_store, &root) {
        Ok(n) => n,
        Err(e) => {
            finalize_failed(handle, "summarize", &e);
            return;
        }
    };

    // Use the actual pending queue depth (includes items from other subtrees that
    // were already pending) so the progress "N / total" ETA is meaningful.
    let enqueued = if newly_enqueued > 0 {
        newly_enqueued
    } else {
        job_store
            .queue_stats()
            .map(|s| s.pending.max(0) as usize)
            .unwrap_or(0)
    };

    push(
        handle,
        JobEvent::Snapshot {
            count: enqueued as u64,
            bytes: 0,
        },
    );

    let mut done = 0usize;
    let mut errors = 0usize;
    let mut samples: std::collections::VecDeque<(std::time::Instant, u64)> =
        std::collections::VecDeque::with_capacity(16);
    samples.push_back((std::time::Instant::now(), 0));

    // Count per-directory defers (children not summarized yet) for the force-rollup cap.
    let mut defers: HashMap<String, u32> = HashMap::new();

    loop {
        // Honor cancellation requested via DELETE /api/jobs/:id.
        if handle.is_cancelled() {
            finalize_cancelled(handle, done);
            return;
        }

        let item = match job_store.next_queue_item() {
            Ok(Some(i)) => i,
            Ok(None) => break,
            Err(e) => {
                finalize_failed(handle, "summarize", &e);
                return;
            }
        };
        let item_path = item.path.clone();
        // Force the roll-up after too many defers so a stuck child can't hang the job.
        let force =
            item.kind == "dir" && defers.get(&item.path).copied().unwrap_or(0) >= MAX_DIR_DEFERS;

        // Watchdog: pause if memory is tight before the LLM summarization call. On a Critical
        // pause we unload the describer LLM and embedder so their RAM frees and we can resume.
        run_watchdog_check(
            &mut wdog,
            &spec,
            headroom,
            handle,
            "summarize",
            Some(embedder.as_ref()),
            Some(&*describer),
        )
        .await;

        let llm_start = std::time::Instant::now();

        // Only stream tokens when someone is watching (receiver_count > 0).
        // This avoids flooding the broadcast channel when no client is connected.
        let r = if handle.tx.receiver_count() > 0 {
            let h = handle.clone();
            let ip = item_path.clone();
            let model_name = if item.kind == "file" {
                cfg.file_model.clone()
            } else {
                cfg.dir_model.clone()
            };
            let stage = if item.kind == "file" {
                "summarize_file".to_owned()
            } else {
                "summarize_dir".to_owned()
            };
            let mut on_frag = move |frag: String| {
                broadcast_only(
                    &h,
                    JobEvent::LlmFragment {
                        item_path: ip.clone(),
                        model: model_name.clone(),
                        stage: stage.clone(),
                        fragment: frag,
                    },
                );
            };
            process_queue_item_with_passes(
                &mut job_store,
                describer.as_ref(),
                embedder.as_ref(),
                &item,
                &cfg,
                passes_override,
                Some(&mut on_frag),
                force,
            )
            .await
        } else {
            process_queue_item_with_passes(
                &mut job_store,
                describer.as_ref(),
                embedder.as_ref(),
                &item,
                &cfg,
                passes_override,
                None,
                force,
            )
            .await
        };
        let llm_secs = llm_start.elapsed().as_secs_f64();
        match r {
            // CompletedUnchanged = the freshness gate skipped the LLM; for job
            // progress both are a completed item (the per-item event stream
            // already shows zero LLM fragments for skips).
            // Orphaned = the row was self-cleaned (path no longer a live entry); terminal,
            // same as a completed item for progress purposes.
            Ok(QueueOutcome::Completed)
            | Ok(QueueOutcome::CompletedUnchanged)
            | Ok(QueueOutcome::Orphaned) => {
                defers.remove(&item.path);
                done += 1;
            }
            Ok(QueueOutcome::Failed) => {
                defers.remove(&item.path);
                errors += 1;
                push(
                    handle,
                    JobEvent::Warning {
                        stage: "summarize".to_owned(),
                        item_path: Some(item.path.clone()),
                        message: "summary generation failed for this file".to_owned(),
                        pressure: None,
                    },
                );
            }
            // Dir children not summarized yet; it was re-enqueued `pending`. Back off and
            // poll again (a deferred dir stays pending, so `break`-on-None won't drop it).
            Ok(QueueOutcome::Deferred) => {
                *defers.entry(item.path.clone()).or_insert(0) += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
            // Err = unexpected store error that left the row `in_flight`. Terminalize it
            // (best-effort) so it can't get stuck blocking the queue.
            Err(e) => {
                defers.remove(&item.path);
                errors += 1;
                push(
                    handle,
                    JobEvent::Warning {
                        stage: "summarize".to_owned(),
                        item_path: Some(item.path.clone()),
                        message: format!("{e:#}"),
                        pressure: None,
                    },
                );
                if let Err(mark_err) =
                    job_store.mark_queue_state(&item.path, "failed", Some(&format!("{e:#}")))
                {
                    tracing::warn!(
                        path = %item.path,
                        error = %mark_err,
                        "summarize: failed to terminalize stuck queue row as failed; it may stay in_flight"
                    );
                }
            }
        }

        let processed = (done + errors) as u64;
        let now = std::time::Instant::now();
        let cutoff = now - std::time::Duration::from_secs(5);
        while samples.len() > 1 && samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            samples.pop_front();
        }
        samples.push_back((now, processed));

        let (rate, eta) = throughput_eta(&samples, processed, enqueued as u64);

        let note = Some(format!("{:.1}s · {}", llm_secs, cfg.file_model));
        push(
            handle,
            JobEvent::Progress {
                current: processed,
                total: enqueued as u64,
                note,
                current_path: Some(item_path),
                items_per_sec: rate,
                eta_secs: eta,
            },
        );
    }

    if done == 0 && errors > 0 {
        // Nothing succeeded AND there were failures (e.g. Ollama went down for the whole run) —
        // report a failure, not a misleading "0 summaries generated" Done.
        finalize_failed(
            handle,
            "summarize",
            &anyhow::anyhow!("all {errors} summary item(s) failed — see indexa status / the log"),
        );
    } else {
        let summary = if errors > 0 {
            format!("{done} summaries generated, {errors} failed — see indexa status")
        } else {
            format!("{done} summaries generated")
        };
        push(handle, JobEvent::Done { summary });
        handle.set_status(JobStatus::Done);
    }
}

#[cfg(test)]
mod tests {
    use super::throughput_eta;
    use std::collections::VecDeque;
    use std::time::Instant;

    #[test]
    fn throughput_eta_needs_two_samples_and_eta_never_underflows() {
        let mut s: VecDeque<(Instant, u64)> = VecDeque::new();
        // Fewer than two samples → no rate/ETA yet.
        assert_eq!(throughput_eta(&s, 0, 100), (None, None));
        s.push_back((Instant::now(), 0));
        assert_eq!(throughput_eta(&s, 5, 100), (None, None));

        // Two samples → Some. Even when `current` exceeds `total` (a pending count drifting as
        // items go in-flight), the saturating remainder keeps ETA non-negative — never a panic.
        s.push_back((Instant::now(), 200));
        let (rate, eta) = throughput_eta(&s, 200, 100);
        assert!(rate.is_some());
        assert!(eta.unwrap() >= 0.0);
    }
}
