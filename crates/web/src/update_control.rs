//! The web→Rust control signal for desktop self-updates (the reverse of [`crate::update_progress`]).
//!
//! The desktop webview loads a remote URL with no Tauri IPC, so when the user clicks "Install" or
//! "Later" in the in-app changelog modal it can't call the desktop directly. Instead `15-update.js`
//! POSTs to `/api/update/control`, the handler calls [`send_command`], and the desktop's
//! `install_update` task — waiting on [`wait_for_command`] — wakes and proceeds. Same process, so a
//! process-global `watch<Option<UpdateCommand>>` carries the signal (mirrors `update_progress`).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    LazyLock,
};
use tokio::sync::watch;

/// What the user chose in the in-app "update available" modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateCommand {
    /// Begin downloading + installing the update.
    Start,
    /// Dismiss — do not download.
    Dismiss,
}

struct ControlChannel {
    tx: watch::Sender<Option<UpdateCommand>>,
    rx: watch::Receiver<Option<UpdateCommand>>,
    pending: AtomicBool,
}

impl ControlChannel {
    fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            tx,
            rx,
            pending: AtomicBool::new(false),
        }
    }

    fn begin_prompt(&self) {
        let _ = self.tx.send(None);
        self.pending.store(true, Ordering::Release);
    }

    fn send(&self, cmd: UpdateCommand) -> bool {
        if self
            .pending
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.tx.send(Some(cmd)).is_ok()
    }
}

static CHANNEL: LazyLock<ControlChannel> = LazyLock::new(ControlChannel::new);

struct PendingPromptGuard;

impl Drop for PendingPromptGuard {
    fn drop(&mut self) {
        CHANNEL.pending.store(false, Ordering::Release);
        let _ = CHANNEL.tx.send(None);
    }
}

/// Mark the update-choice prompt active before publishing it to the webview.
pub fn begin_update_prompt() {
    CHANNEL.begin_prompt();
}

/// Publish the user's choice from the webview (called by the `/api/update/control` handler).
pub fn send_command(cmd: UpdateCommand) -> bool {
    CHANNEL.send(cmd)
}

/// Await the user choice for the active prompt, then reset the channel to `None`.
pub async fn wait_for_command() -> UpdateCommand {
    let _pending_guard = PendingPromptGuard;
    let mut rx = CHANNEL.rx.clone();
    loop {
        // Copy the value out and DROP the borrow guard before sending — holding a watch read
        // guard across `send` on the same task would deadlock (send needs the write lock).
        let current = *rx.borrow_and_update();
        if let Some(cmd) = current {
            let _ = CHANNEL.tx.send(None);
            return cmd;
        }
        if rx.changed().await.is_err() {
            // Sender is static and never dropped, so this is unreachable in practice; treat a
            // closed channel as a dismiss rather than spinning.
            return UpdateCommand::Dismiss;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_resolves_to_sent_command_then_resets() {
        // Send before waiting — wait_for_command should pick it up immediately.
        begin_update_prompt();
        assert!(send_command(UpdateCommand::Start));
        let cmd = wait_for_command().await;
        assert_eq!(cmd, UpdateCommand::Start);
        // After consuming, the channel is reset to None (a fresh wait would block) — verify the
        // latest value is None so a stale Start can't re-trigger.
        assert!(CHANNEL.rx.borrow().is_none());
    }

    #[tokio::test]
    async fn command_without_pending_prompt_is_dropped() {
        let channel = ControlChannel::new();
        assert!(!channel.send(UpdateCommand::Start));
        channel.begin_prompt();
        assert!(
            channel.rx.borrow().is_none(),
            "stale command must be cleared"
        );
        let mut rx = channel.rx.clone();
        let waiting = tokio::spawn(async move {
            loop {
                if let Some(cmd) = *rx.borrow_and_update() {
                    break cmd;
                }
                if rx.changed().await.is_err() {
                    break UpdateCommand::Dismiss;
                }
            }
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "the rejected stale command must not answer the prompt"
        );
        assert!(channel.send(UpdateCommand::Dismiss));
        assert_eq!(waiting.await.unwrap(), UpdateCommand::Dismiss);
    }
}
