//! The web→Rust control signal for desktop self-updates (the reverse of [`crate::update_progress`]).
//!
//! The desktop webview loads a remote URL with no Tauri IPC, so when the user clicks "Install" or
//! "Later" in the in-app changelog modal it can't call the desktop directly. Instead `15-update.js`
//! POSTs to `/api/update/control`, the handler calls [`send_command`], and the desktop's
//! `install_update` task — waiting on [`wait_for_command`] — wakes and proceeds. Same process, so a
//! process-global `watch<Option<UpdateCommand>>` carries the signal (mirrors `update_progress`).

use std::sync::LazyLock;
use tokio::sync::watch;

/// What the user chose in the in-app "update available" modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateCommand {
    /// Begin downloading + installing the update.
    Start,
    /// Dismiss — do not download.
    Dismiss,
}

type ControlChannel = (
    watch::Sender<Option<UpdateCommand>>,
    watch::Receiver<Option<UpdateCommand>>,
);
static CHANNEL: LazyLock<ControlChannel> = LazyLock::new(|| watch::channel(None));

/// Publish the user's choice from the webview (called by the `/api/update/control` handler).
pub fn send_command(cmd: UpdateCommand) {
    let _ = CHANNEL.0.send(Some(cmd));
}

/// Await the next user choice, then reset the channel to `None` so a subsequent call blocks again.
///
/// The desktop's update task calls this once per "update available" cycle. Returns when the value
/// transitions to `Some(cmd)`; the reset-to-`None` is what makes the next cycle wait afresh (without
/// it, a stale command would resolve immediately).
pub async fn wait_for_command() -> UpdateCommand {
    let mut rx = CHANNEL.1.clone();
    loop {
        // Copy the value out and DROP the borrow guard before sending — holding a watch read
        // guard across `send` on the same task would deadlock (send needs the write lock).
        let current = *rx.borrow_and_update();
        if let Some(cmd) = current {
            let _ = CHANNEL.0.send(None);
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
        send_command(UpdateCommand::Start);
        let cmd = wait_for_command().await;
        assert_eq!(cmd, UpdateCommand::Start);
        // After consuming, the channel is reset to None (a fresh wait would block) — verify the
        // latest value is None so a stale Start can't re-trigger.
        assert!(CHANNEL.1.borrow().is_none());
    }
}
