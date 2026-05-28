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
    Note {
        msg: String,
    },
    Done {
        summary: String,
    },
    Failed {
        error: String,
    },
}

pub struct JobHandle {
    pub id: Uuid,
    pub kind: String,
    pub path: String,
    pub started_at: i64,
    pub status: Mutex<JobStatus>,
    pub history: Mutex<Vec<JobEvent>>,
    pub tx: broadcast::Sender<JobEvent>,
}

impl JobHandle {
    pub fn new(kind: impl Into<String>, path: impl Into<String>) -> Self {
        let (tx, _) = broadcast::channel(64);
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
        }
    }
}

/// Shared jobs registry.
pub type Jobs = Arc<RwLock<HashMap<Uuid, Arc<JobHandle>>>>;

/// Push an event into a job's history and broadcast it to subscribers.
pub fn push(handle: &Arc<JobHandle>, event: JobEvent) {
    handle.history.lock().unwrap().push(event.clone());
    let _ = handle.tx.send(event);
}
