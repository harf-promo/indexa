use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    Start {
        kind: String,
        path: String,
        total: Option<u64>,
    },
    /// Emitted once after the file-list snapshot is complete, before processing begins.
    Snapshot {
        count: u64,
        bytes: u64,
    },
    Progress {
        current: u64,
        total: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        items_per_sec: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        eta_secs: Option<f64>,
    },
    Done {
        summary: String,
    },
    Failed {
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        item_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        chain: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    /// A non-fatal issue that did not stop the job (e.g. one file failed to parse).
    Warning {
        stage: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        item_path: Option<String>,
        message: String,
        /// Structured memory-pressure context, present only on the watchdog's
        /// "easing off" warnings. Lets the UI correlate the warning with the live
        /// RAM gauge instead of parsing the prose. `None` for all other warnings.
        ///
        /// This is an added FIELD, not a new variant, on purpose: the frontend
        /// dispatches on `ev.type`, so a new variant would be silently dropped,
        /// whereas an extra optional field is ignored by older clients.
        #[serde(skip_serializing_if = "Option::is_none")]
        pressure: Option<PressureInfo>,
    },
    /// A fragment of LLM output streamed in real time.
    /// NOT stored in job history — broadcast-only to avoid unbounded memory growth.
    LlmFragment {
        item_path: String,
        model: String,
        stage: String,
        fragment: String,
    },
}

/// Machine-memory snapshot attached to a watchdog "easing off" warning, so the UI
/// can show *why* a build paused (and line it up with the live Engine-bar gauge)
/// rather than scraping the message text. Every value is already computed in the
/// watchdog when the warning fires.
#[derive(Debug, Clone, Serialize)]
pub struct PressureInfo {
    /// "throttle" | "critical" — the `assess()` level at the moment of the warning.
    pub level: String,
    /// Swap used as a percent of total swap (0–100).
    pub swap_percent: u64,
    /// Active+wired bytes in use (cache-excluded), the budget's `used` term.
    pub used_bytes: u64,
    /// `compute_budget` = free RAM for a model load, minus headroom. Negative = over budget.
    pub budget_bytes: i64,
    /// The configured keep-free margin the budget subtracts.
    pub headroom_bytes: u64,
}

pub struct JobHandle {
    pub id: Uuid,
    pub kind: String,
    pub path: String,
    pub started_at: i64,
    pub status: Mutex<JobStatus>,
    pub history: Mutex<Vec<JobEvent>>,
    /// The single most recent `Progress` event, stored separately from `history`.
    /// `Progress` fires roughly once per file (hundreds of thousands for a large
    /// scan) and no client ever reads more than the latest one, so it is never
    /// appended to `history` — see `push` and `history_snapshot`.
    pub last_progress: Mutex<Option<JobEvent>>,
    pub tx: broadcast::Sender<JobEvent>,
    /// Set true to request the running job stop at the next loop iteration.
    pub cancelled: std::sync::atomic::AtomicBool,
}

impl JobHandle {
    pub fn new(kind: impl Into<String>, path: impl Into<String>) -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            id: Uuid::new_v4(),
            kind: kind.into(),
            path: path.into(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            status: Mutex::new(JobStatus::Running),
            history: Mutex::new(Vec::new()),
            last_progress: Mutex::new(None),
            tx,
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// True if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Poison-safe read (clone) of the current job status. Recovers the inner value
    /// if another thread panicked while holding the lock, so one job's panic can't
    /// poison the mutex and take every other job's status read down with it.
    pub fn status(&self) -> JobStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Poison-safe write of the job status.
    pub fn set_status(&self, status: JobStatus) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    /// Poison-safe snapshot (clone) of the job's event history, including the
    /// latest `Progress` event (which `push` keeps out of `history` proper —
    /// see its doc comment) appended at the end so readers still see where a
    /// running job currently stands.
    ///
    /// The append only happens while the job is still `Running`, AND only if
    /// `history` doesn't already end in a terminal event (`Done`/`Failed`).
    /// The second check closes a narrow race: `finalize_done`/`finalize_failed`
    /// push the terminal event into `history` and only *then* call
    /// `set_status`, so there's a brief window where `history` already ends in
    /// `Done`/`Failed` but `status()` still reads `Running`. Without the check,
    /// a snapshot taken in that window would append a stale `Progress` after
    /// the terminal event, making a replay stream look like `Done` then
    /// `Progress` — i.e. the job appears to restart after finishing.
    pub fn history_snapshot(&self) -> Vec<JobEvent> {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ends_in_terminal = matches!(
            history.last(),
            Some(JobEvent::Done { .. }) | Some(JobEvent::Failed { .. })
        );
        if self.status() == JobStatus::Running && !ends_in_terminal {
            if let Some(progress) = self
                .last_progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                history.push(progress);
            }
        }
        history
    }
}

/// Shared jobs registry.
pub type Jobs = Arc<RwLock<HashMap<Uuid, Arc<JobHandle>>>>;

/// Maximum number of Warning events stored in job history.
/// Older warnings are dropped when this cap is reached.
pub const MAX_STORED_WARNINGS: usize = 500;

/// Push an event into a job's history and broadcast it to subscribers.
///
/// `Progress` events are never stored in `history`: they fire roughly once per
/// file processed (hundreds of thousands of events for a large scan) and no
/// client ever reads more than the latest one. Instead the latest `Progress`
/// overwrites `handle.last_progress`; `history_snapshot` folds it back in for
/// readers. All other event kinds are appended to `history` as before.
///
/// Warning events are capped at `MAX_STORED_WARNINGS` to bound memory.
/// The true count can be recovered from `stageCounts` on the client.
pub fn push(handle: &Arc<JobHandle>, event: JobEvent) {
    if matches!(event, JobEvent::Progress { .. }) {
        *handle
            .last_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(event.clone());
    } else {
        let mut history = handle.history.lock().unwrap_or_else(|e| e.into_inner());
        // For Warning events: cap stored history to avoid unbounded growth.
        if matches!(event, JobEvent::Warning { .. }) {
            let warn_count = history
                .iter()
                .filter(|e| matches!(e, JobEvent::Warning { .. }))
                .count();
            if warn_count >= MAX_STORED_WARNINGS {
                // Drop the oldest warning to make room.
                if let Some(pos) = history
                    .iter()
                    .position(|e| matches!(e, JobEvent::Warning { .. }))
                {
                    history.remove(pos);
                }
            }
        }
        history.push(event.clone());
    }
    let _ = handle.tx.send(event);
}

/// Broadcast an event to live subscribers WITHOUT storing it in history.
/// Use for high-volume streaming events (e.g. LlmFragment) to avoid memory bloat.
pub fn broadcast_only(handle: &Arc<JobHandle>, event: JobEvent) {
    let _ = handle.tx.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(current: u64) -> JobEvent {
        JobEvent::Progress {
            current,
            total: 1000,
            note: None,
            current_path: None,
            items_per_sec: None,
            eta_secs: None,
        }
    }

    /// Pushing many `Progress` events must not grow `history` unbounded — only the
    /// latest one should surface, and only via `history_snapshot`, while the job
    /// is still running.
    #[test]
    fn progress_events_do_not_grow_history() {
        let handle = Arc::new(JobHandle::new("scan", "/tmp/x"));
        for i in 0..1000u64 {
            push(&handle, progress(i));
        }

        // Nothing ever landed in `history` proper — it should still be empty.
        assert!(handle.history_snapshot().len() <= 1);

        let snapshot = handle.history_snapshot();
        let last = snapshot.last().expect("expected the folded-in Progress");
        match last {
            JobEvent::Progress { current, .. } => assert_eq!(*current, 999),
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    /// After a job finishes, `history_snapshot` must end with the terminal event —
    /// never a stale `Progress` appended after it (which would make a replay
    /// stream look like the job restarted).
    #[test]
    fn terminal_event_is_never_followed_by_stale_progress() {
        let handle = Arc::new(JobHandle::new("scan", "/tmp/x"));
        push(&handle, progress(1));
        push(&handle, progress(2));
        push(
            &handle,
            JobEvent::Done {
                summary: "done".into(),
            },
        );
        handle.set_status(JobStatus::Done);

        let snapshot = handle.history_snapshot();
        match snapshot.last().expect("expected at least the Done event") {
            JobEvent::Done { summary } => assert_eq!(summary, "done"),
            other => panic!("expected Done as the last event, got {other:?}"),
        }
        // No Progress event should appear anywhere in the terminal snapshot —
        // distinguishes "appended in the wrong place" from "correctly dropped".
        assert!(
            !snapshot
                .iter()
                .any(|e| matches!(e, JobEvent::Progress { .. })),
            "terminal snapshot must not contain any Progress event: {snapshot:?}"
        );
    }

    /// Mirrors the real call sequence used by job execution (`finalize_failed` in
    /// `jobs_exec`): push the terminal event, THEN flip status. `history_snapshot`
    /// taken in between must not append a stale Progress after `Failed` either.
    #[test]
    fn failed_terminal_event_is_never_followed_by_stale_progress() {
        let handle = Arc::new(JobHandle::new("deep", "/tmp/x"));
        push(&handle, progress(5));
        push(
            &handle,
            JobEvent::Failed {
                error: "boom".into(),
                stage: None,
                item_path: None,
                chain: None,
                code: None,
            },
        );
        // Snapshot taken in the exact window where `history` already ends in
        // Failed but status() hasn't been flipped yet — this is the race the
        // `ends_in_terminal` guard in `history_snapshot` exists to close.
        let snapshot = handle.history_snapshot();
        assert!(matches!(snapshot.last(), Some(JobEvent::Failed { .. })));

        handle.set_status(JobStatus::Failed);
        let snapshot = handle.history_snapshot();
        assert!(matches!(snapshot.last(), Some(JobEvent::Failed { .. })));
    }

    /// The existing Warning-cap behavior must survive the `push` restructuring
    /// (Progress now takes an early-return branch that Warning must not hit).
    #[test]
    fn warning_events_are_still_capped_and_oldest_dropped() {
        let handle = Arc::new(JobHandle::new("scan", "/tmp/x"));
        for i in 0..(MAX_STORED_WARNINGS + 10) {
            push(
                &handle,
                JobEvent::Warning {
                    stage: "parse".into(),
                    item_path: Some(format!("file-{i}.txt")),
                    message: "oops".into(),
                    pressure: None,
                },
            );
        }

        let snapshot = handle.history_snapshot();
        let warnings: Vec<&JobEvent> = snapshot
            .iter()
            .filter(|e| matches!(e, JobEvent::Warning { .. }))
            .collect();
        assert_eq!(warnings.len(), MAX_STORED_WARNINGS);
        // The oldest 10 warnings (file-0..file-9) should have been dropped, so
        // the earliest surviving one is file-10.
        match warnings.first().unwrap() {
            JobEvent::Warning { item_path, .. } => {
                assert_eq!(item_path.as_deref(), Some("file-10.txt"));
            }
            _ => unreachable!(),
        }
    }

    /// `broadcast_only` must remain a pure live-broadcast path that never touches
    /// `history` or `last_progress`.
    #[test]
    fn broadcast_only_does_not_touch_history_or_last_progress() {
        let handle = Arc::new(JobHandle::new("scan", "/tmp/x"));
        broadcast_only(
            &handle,
            JobEvent::LlmFragment {
                item_path: "a.txt".into(),
                model: "gemma3:4b".into(),
                stage: "describe".into(),
                fragment: "hello".into(),
            },
        );
        assert!(handle.history_snapshot().is_empty());
        assert!(handle
            .last_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none());
    }
}
