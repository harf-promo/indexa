//! Claude Code (subscription) adapter — uses the user's Claude Pro/Max plan by
//! shelling out to the installed `claude` CLI in headless print mode, instead of
//! the metered Anthropic API. No API key, no token billing: as long as the user
//! is logged into Claude Code on this machine, `claude -p` runs on their plan.
//!
//! Command shape (probed against claude CLI v2.1.158):
//!   `claude -p "<prompt>" --output-format json --model <model>`
//! The JSON result is one object whose `result` field holds the text and
//! `is_error` flags a failure (e.g. `result:"Not logged in · ..."`).
//! NOTE: `--bare` is deliberately NOT used — probing showed it strips the
//! auth/settings sources and yields "Not logged in", defeating the whole point.
//! Plain `-p` reuses the logged-in Claude Code session on the user's plan.
//!
//! Trade-off vs local Ollama: each call spawns a fresh `claude` process (~6 s
//! startup), so a [`tokio::sync::Semaphore`] caps concurrent processes to keep
//! bulk summarization from forking dozens of CLIs at once. For whole-disk bulk,
//! local Ollama is faster; for `ask` and targeted summaries, Sonnet quality wins.

use crate::{ChildSummary, Describer, Generator};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Default cap on concurrent `claude` subprocesses. Each call is heavy (a full
/// CLI session spin-up), so we keep this small; the summary worker's own
/// concurrency multiplies against it.
const DEFAULT_MAX_CONCURRENCY: usize = 3;

/// Hard timeout for a single `claude -p` invocation. A normal call is ~6 s; without a
/// cap a hung CLI process would stall a summarize/answer job indefinitely. Generous
/// enough for a large summary prompt, bounded so a stuck process always terminalizes.
const CLAUDE_TIMEOUT_SECS: u64 = 300;

/// The subset of `claude -p --output-format json` we care about.
#[derive(Deserialize)]
struct ClaudeCliResult {
    #[serde(default)]
    result: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: String,
}

/// LLM adapter backed by the `claude` CLI on the user's subscription.
pub struct ClaudeCodeLlm {
    /// Model for Q&A answer synthesis (e.g. "sonnet").
    model: String,
    /// Model for per-file summaries.
    file_model: String,
    /// Model for directory roll-ups.
    dir_model: String,
    /// Path to the `claude` binary (default "claude", resolved on PATH).
    claude_bin: String,
    /// Limits concurrent `claude` subprocesses.
    sem: Arc<Semaphore>,
}

impl ClaudeCodeLlm {
    /// Build with explicit models. `claude_bin` empty → defaults to "claude".
    pub fn new(
        model: impl Into<String>,
        file_model: impl Into<String>,
        dir_model: impl Into<String>,
        claude_bin: Option<&str>,
    ) -> Self {
        let bin = claude_bin
            .filter(|s| !s.is_empty())
            .unwrap_or("claude")
            .to_string();
        Self {
            model: model.into(),
            file_model: file_model.into(),
            dir_model: dir_model.into(),
            claude_bin: bin,
            sem: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENCY)),
        }
    }

    /// Single-model convenience (Q&A path): file/dir models mirror `model`.
    pub fn single(model: impl Into<String>, claude_bin: Option<&str>) -> Self {
        let m = model.into();
        Self::new(m.clone(), m.clone(), m, claude_bin)
    }

    /// Run one prompt through `claude -p` on the given model, return its text.
    async fn run(&self, prompt: &str, model: &str) -> Result<String> {
        let _permit = self
            .sem
            .acquire()
            .await
            .expect("claude-code semaphore never closed");

        let run_fut = Command::new(&self.claude_bin)
            .arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg(model)
            // Belt-and-suspenders: never let an inherited API key silently switch
            // this off the subscription and onto metered billing.
            .env_remove("ANTHROPIC_API_KEY")
            // Kill the child if we drop the future on timeout, so a hung `claude` can't
            // leak as a zombie holding the concurrency permit.
            .kill_on_drop(true)
            .output();
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(CLAUDE_TIMEOUT_SECS), run_fut)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "claude CLI timed out after {CLAUDE_TIMEOUT_SECS}s (model {model})"
                    )
                })?
                .with_context(|| {
                    format!(
                        "failed to spawn `{}` — is the Claude Code CLI installed and on PATH?",
                        self.claude_bin
                    )
                })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "claude CLI exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let parsed: ClaudeCliResult = serde_json::from_slice(&output.stdout)
            .context("could not parse `claude -p --output-format json` output")?;

        if parsed.is_error {
            bail!("claude CLI reported an error (subtype: {})", parsed.subtype);
        }

        Ok(parsed.result.trim().to_string())
    }
}

#[async_trait::async_trait]
impl Generator for ClaudeCodeLlm {
    async fn generate(&self, prompt: &str) -> Result<String> {
        self.run(prompt, &self.model).await
    }
    // generate_stream: default buffered fallback (the CLI's stream-json adds
    // complexity for no real win here; cloud adapters don't stream either).
    // unload: default no-op (no local model resident).
}

#[async_trait::async_trait]
impl Describer for ClaudeCodeLlm {
    fn provider_name(&self) -> &'static str {
        "claude-code"
    }

    async fn describe(
        &self,
        path: &str,
        content_sample: &[u8],
        previous_summary: Option<&str>,
    ) -> Result<String> {
        // Same prompt shape as the Ollama describer so summary style is consistent.
        let sample = String::from_utf8_lossy(content_sample);
        let sample = sample.chars().take(800).collect::<String>();
        let prompt = match previous_summary {
            Some(prev) => format!(
                "We have provided an existing summary up to a certain point: {prev}\n\
                 We have the opportunity to refine the existing summary (only if needed) with some more \
                 context below.\n------------\n{sample}\n------------\n\
                 Given the new context, refine the original summary. If the context isn't useful, return \
                 the original summary.\nFile: {path}",
            ),
            None => format!(
                "Briefly describe what this file is about in 1-2 sentences.\nFile: {path}\nContent:\n{sample}",
            ),
        };
        let prompt = format!("{prompt}\n\n{}", crate::SUMMARY_OUTPUT_RULE);
        self.run(&prompt, &self.file_model).await
    }

    async fn summarize_dir(
        &self,
        dir_path: &str,
        children: &[ChildSummary],
        previous_summary: Option<&str>,
    ) -> Result<String> {
        let mut child_text = String::new();
        for c in children.iter().take(30) {
            child_text.push_str(&format!("- {} ({}): {}\n", c.name, c.kind, c.summary));
        }
        let prompt = match previous_summary {
            Some(prev) => format!(
                "We have an existing directory summary: {prev}\n\
                 Refine it (only if needed) given the child entries below.\n{child_text}\n\
                 Directory: {dir_path}",
            ),
            None => format!(
                "Summarize what this directory contains in 2-4 sentences based on its children.\n\
                 {child_text}\nDirectory: {dir_path}",
            ),
        };
        let prompt = format!("{prompt}\n\n{}", crate::SUMMARY_OUTPUT_RULE);
        self.run(&prompt, &self.dir_model).await
    }
    // describe_stream / summarize_dir_stream: default buffered fallback.
    // unload: default no-op.
}

// ── CLI status probe ────────────────────────────────────────────────────────────

/// Result of probing the local `claude` CLI for presence + subscription login.
///
/// Both sub-checks are **token-free** — they never invoke a model: `--version`
/// for presence and `auth status --json` for the logged-in session. Safe to call
/// on a web request or in `doctor`. A missing binary yields `cli_present = false`
/// with the rest left at their defaults.
#[derive(Debug, Clone, Default)]
pub struct ClaudeStatus {
    /// The `claude` binary resolved and `--version` exited successfully.
    pub cli_present: bool,
    /// Leading version token from `claude --version` (e.g. `"2.1.158"`).
    pub cli_version: Option<String>,
    /// `claude auth status` reports a logged-in session.
    pub logged_in: bool,
    /// Auth method, e.g. `"claude.ai"` (subscription) — from `auth status`.
    pub auth_method: Option<String>,
    /// Subscription tier, e.g. `"max"` / `"pro"` — from `auth status`.
    pub subscription_type: Option<String>,
}

/// Subset of `claude auth status --json` we read. Intentionally omits the CLI's
/// `email` / `orgId` / `orgName` fields (PII) — serde drops un-named fields, so
/// they never reach [`ClaudeStatus`] or the web DTO. Do NOT add them.
#[derive(Deserialize)]
struct ClaudeAuthStatus {
    #[serde(default, rename = "loggedIn")]
    logged_in: bool,
    #[serde(default, rename = "authMethod")]
    auth_method: Option<String>,
    #[serde(default, rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Probe the `claude` CLI for presence and subscription login state.
///
/// Runs two local, token-free commands: `<bin> --version` and
/// `<bin> auth status --json`. Never invokes a model, so it's cheap enough to
/// call on every Settings load or `doctor` run. Returns whatever could be
/// determined; if the binary is absent, auth is not probed.
pub async fn claude_status(claude_bin: &str) -> ClaudeStatus {
    let bin = if claude_bin.is_empty() {
        "claude"
    } else {
        claude_bin
    };
    let mut status = ClaudeStatus::default();

    // Hard cap each probe: `auth status` may touch the network to validate the
    // OAuth token, and this runs on a web request (Settings load). A stalled CLI
    // must degrade to "unknown", never hang the Axum worker.
    let probe_timeout = std::time::Duration::from_secs(5);

    // Presence: `claude --version` → e.g. "2.1.158 (Claude Code)".
    let version = tokio::time::timeout(
        probe_timeout,
        Command::new(bin)
            .arg("--version")
            .env_remove("ANTHROPIC_API_KEY")
            // As in `run()`: if the timeout fires and this future is dropped, kill the
            // child instead of leaking it detached — a probe run on every Settings load
            // must not accumulate orphaned `claude` processes.
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if let Ok(Ok(out)) = version {
        if out.status.success() {
            status.cli_present = true;
            let v = String::from_utf8_lossy(&out.stdout);
            status.cli_version = v.split_whitespace().next().map(|s| s.to_owned());
        }
    }

    // No binary → nothing more to learn.
    if !status.cli_present {
        return status;
    }

    // Login state: `claude auth status --json` →
    // {"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"max",...}.
    // `--json` is the default but we pass it explicitly to be robust to changes.
    let auth = tokio::time::timeout(
        probe_timeout,
        Command::new(bin)
            .arg("auth")
            .arg("status")
            .arg("--json")
            .env_remove("ANTHROPIC_API_KEY")
            // Same reasoning as the `--version` probe above: this one may touch the
            // network (OAuth validation), making a timeout-then-leak more likely, not less.
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if let Ok(Ok(out)) = auth {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Ok(parsed) = serde_json::from_str::<ClaudeAuthStatus>(&stdout) {
            status.logged_in = parsed.logged_in;
            status.auth_method = parsed.auth_method;
            status.subscription_type = parsed.subscription_type;
        }
    }

    status
}

// Linux-only: the only test in this module needs `/proc/<pid>` (see its own doc comment), so
// the whole module — not just the function — is gated on `target_os = "linux"`. Gating only the
// function would leave `use super::*` unused (and thus a hard clippy error under `-D warnings`)
// on any other CI target, since nothing else in the module would consume it.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Regression test for a missing `.kill_on_drop(true)` on the `claude_status` probes: when
    /// the 5s probe timeout fires and `tokio::time::timeout` drops the still-running child
    /// future, the underlying process must actually die, not keep running detached. This runs
    /// on every Settings-page load and every `doctor` run, so a leak here accumulates one
    /// orphaned process per timed-out probe.
    ///
    /// Checks liveness via `/proc/<pid>` rather than pulling in a new dependency just for this
    /// test (mirrors the `#[cfg(unix)]`-gated platform tests already used elsewhere in this
    /// codebase, e.g. `crates/core/src/walker.rs`).
    #[tokio::test]
    async fn timed_out_version_probe_actually_kills_the_child_process() {
        use std::os::unix::fs::PermissionsExt;

        // A fake "claude" binary that outlives the probe's 5s timeout by a wide margin, and
        // records its own pid so the test can check afterward whether it's still alive.
        let dir = std::env::temp_dir().join(format!(
            "indexa-claude-status-kill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > {}\nsleep 30\n", pid_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        claude_status(script.to_str().unwrap()).await;

        // Give the OS a brief moment to finish reaping the killed child after `output()` returns.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let pid = std::fs::read_to_string(&pid_file)
            .expect("fake claude script should have written its pid before the 5s timeout")
            .trim()
            .to_owned();
        let still_alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        assert!(
            !still_alive,
            "probe subprocess pid {pid} was not killed when the timeout dropped its future"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
