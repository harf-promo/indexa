//! The one way Indexa spawns `git`.
//!
//! Indexa runs `git` inside directories it merely indexes, so a repository's own
//! `.git/config` is untrusted input. Several config keys make git execute a program
//! (`core.fsmonitor` on any `diff`/`status` that refreshes the index, `core.hooksPath` for
//! hooks). Every git child the tool spawns is built here so those two are neutralised on the
//! command line, which outranks the repository config. Callers add their own `-C <dir>` and
//! subcommand arguments after this.
//!
//! Option injection through refs is a separate guard (`gitdiff::validate_ref`,
//! `--end-of-options`); `--no-ext-diff`/`--no-textconv` stay at their call site.

use std::process::{Command, Stdio};

/// The `-c key=value` pairs prepended to every git invocation (each preceded by `-c`).
/// Passing `core.hooksPath=/dev/null` mirrors git's own convention for "no hooks".
pub const HARDENING_CONFIG: [&str; 4] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "safe.bareRepository=explicit",
    "log.showSignature=false",
];

/// A [`Command`] for `git` with [`HARDENING_CONFIG`] already applied. Use instead of
/// `Command::new("git")` for anything run against a user-indexed directory.
pub fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for kv in HARDENING_CONFIG {
        cmd.arg("-c").arg(kv);
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_command_prepends_the_hardening_config_before_any_caller_argument() {
        let mut cmd = git_command();
        cmd.arg("-C").arg("/x").arg("status");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "safe.bareRepository=explicit",
                "-c",
                "log.showSignature=false",
                "-C",
                "/x",
                "status"
            ]
        );
        assert_eq!(cmd.get_program(), "git");
        let envs: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs
            .iter()
            .any(|(k, v)| k == "GIT_OPTIONAL_LOCKS" && v.as_deref() == Some("0")));
        assert!(envs
            .iter()
            .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v.as_deref() == Some("0")));
    }
}
