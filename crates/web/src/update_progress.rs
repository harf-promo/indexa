//! Live progress for desktop self-updates and CLI-tool installs.
//!
//! The desktop app and this embedded web server run in the **same process**, but the webview
//! loads a remote URL (`http://localhost:<port>`) and has no Tauri IPC — so `app.emit()` can't
//! reach the page. Instead the desktop publishes progress snapshots here via
//! [`report_update_progress`], and the webview streams them over
//! `GET /api/update/progress/stream` (see [`crate::handlers`]) to render a live bar.
//!
//! The channel is a process-global `watch` (one update at a time per process), so `serve()`'s
//! signature stays unchanged and plain `indexa serve` is unaffected: with no desktop writer the
//! value stays `idle` and the web overlay never appears.

use serde::Serialize;
use std::sync::LazyLock;
use tokio::sync::watch;

/// A snapshot of an in-progress desktop self-update or CLI-tool install, streamed to the webview.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateProgress {
    /// `idle` | `downloading` | `installing` | `done` | `error`.
    pub phase: String,
    /// Human label for what's updating, e.g. `"Indexa 0.30.0"` or `"Command-line tool"`.
    pub title: String,
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Total bytes when the server sent `Content-Length`; `None` → render an indeterminate bar.
    pub total: Option<u64>,
    /// Error message, set only when `phase == "error"`.
    pub error: Option<String>,
}

impl UpdateProgress {
    /// The resting state — no update in flight. The web overlay treats this as "hide".
    pub fn idle() -> Self {
        Self {
            phase: "idle".to_owned(),
            title: String::new(),
            downloaded: 0,
            total: None,
            error: None,
        }
    }

    /// Downloading: `downloaded` of `total` bytes (None total → indeterminate bar).
    pub fn downloading(title: impl Into<String>, downloaded: u64, total: Option<u64>) -> Self {
        Self {
            phase: "downloading".to_owned(),
            title: title.into(),
            downloaded,
            total,
            error: None,
        }
    }

    /// Download finished; unpacking / replacing the binary.
    pub fn installing(title: impl Into<String>) -> Self {
        Self {
            phase: "installing".to_owned(),
            title: title.into(),
            downloaded: 0,
            total: None,
            error: None,
        }
    }

    /// Done — the caller is about to restart (app update) or has written the file (CLI install).
    pub fn done(title: impl Into<String>) -> Self {
        Self {
            phase: "done".to_owned(),
            title: title.into(),
            downloaded: 0,
            total: None,
            error: None,
        }
    }

    /// Failed — `message` is shown in the overlay with a dismiss button.
    pub fn error(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            phase: "error".to_owned(),
            title: title.into(),
            downloaded: 0,
            total: None,
            error: Some(message.into()),
        }
    }
}

static CHANNEL: LazyLock<(
    watch::Sender<UpdateProgress>,
    watch::Receiver<UpdateProgress>,
)> = LazyLock::new(|| watch::channel(UpdateProgress::idle()));

/// Publish an update-progress snapshot to any connected SSE subscribers (the desktop webview).
/// Best-effort: with no listeners the value is simply retained as the channel's latest.
pub fn report_update_progress(p: UpdateProgress) {
    let _ = CHANNEL.0.send(p);
}

/// A fresh receiver for the SSE stream handler. `WatchStream` yields the latest value to each
/// new subscriber, so a reconnecting client immediately sees the current phase.
pub(crate) fn subscribe() -> watch::Receiver<UpdateProgress> {
    CHANNEL.1.clone()
}
