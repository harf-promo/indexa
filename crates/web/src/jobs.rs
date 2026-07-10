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

    /// Poison-safe snapshot (clone) of the job's event history.
    pub fn history_snapshot(&self) -> Vec<JobEvent> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Shared jobs registry.
pub type Jobs = Arc<RwLock<HashMap<Uuid, Arc<JobHandle>>>>;

/// Maximum number of Warning events stored in job history.
/// Older warnings are dropped when this cap is reached.
pub const MAX_STORED_WARNINGS: usize = 500;

/// Push an event into a job's history and broadcast it to subscribers.
///
/// Warning events are capped at `MAX_STORED_WARNINGS` to bound memory.
/// The true count can be recovered from `stageCounts` on the client.
pub fn push(handle: &Arc<JobHandle>, event: JobEvent) {
    {
        let mut history = handle.history.lock().unwrap_or_else(|e| e.into_inner());
        match &event {
            // Cap stored Warnings so a noisy job can't grow history unboundedly.
            JobEvent::Warning { .. } => {
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
                history.push(event.clone());
            }
            // Coalesce a run of Progress events (they're cumulative current/total): overwrite the
            // last one in place instead of storing one-per-file, so a long deep doesn't keep — and
            // re-clone on every SSE reconnect — thousands of Progress events. A Warning between two
            // Progress breaks the run, so the surviving count is bounded by the warning cap.
            JobEvent::Progress { .. }
                if matches!(history.last(), Some(JobEvent::Progress { .. })) =>
            {
                *history.last_mut().unwrap() = event.clone();
            }
            _ => history.push(event.clone()),
        }
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
            total: 100,
            note: None,
            current_path: None,
            items_per_sec: None,
            eta_secs: None,
        }
    }
    fn warning(msg: &str) -> JobEvent {
        JobEvent::Warning {
            stage: "deep".to_owned(),
            item_path: None,
            message: msg.to_owned(),
            pressure: None,
        }
    }
    fn count_progress(h: &JobHandle) -> usize {
        h.history_snapshot()
            .iter()
            .filter(|e| matches!(e, JobEvent::Progress { .. }))
            .count()
    }

    #[test]
    fn push_coalesces_consecutive_progress() {
        // A long deep emits one Progress per file; history must not grow with it.
        let h = Arc::new(JobHandle::new("deep", "/x"));
        for i in 1..=1000 {
            push(&h, progress(i));
        }
        assert_eq!(
            count_progress(&h),
            1,
            "1000 consecutive Progress collapse to one"
        );
        // The surviving Progress is the latest cumulative value.
        match h
            .history_snapshot()
            .into_iter()
            .rev()
            .find(|e| matches!(e, JobEvent::Progress { .. }))
            .unwrap()
        {
            JobEvent::Progress { current, .. } => assert_eq!(current, 1000),
            _ => unreachable!(),
        }
    }

    #[test]
    fn push_progress_run_broken_by_warning_is_bounded_by_warnings() {
        // A Warning between two Progress runs keeps the boundary Progress, so the surviving Progress
        // count tracks the (capped) warning count, not the file count.
        let h = Arc::new(JobHandle::new("deep", "/x"));
        push(&h, progress(1));
        push(&h, progress(2)); // coalesces into progress(1)
        push(&h, warning("w"));
        push(&h, progress(3)); // new run after the warning
        push(&h, progress(4)); // coalesces into progress(3)
        assert_eq!(count_progress(&h), 2);
    }
}
