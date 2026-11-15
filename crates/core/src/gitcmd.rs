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

use std::process::Command;

/// The `-c key=value` pairs prepended to every git invocation (each preceded by `-c`).
/// Passing `core.hooksPath=/dev/null` mirrors git's own convention for "no hooks".
pub const HARDENING_CONFIG: [&str; 2] = ["core.fsmonitor=false", "core.hooksPath=/dev/null"];

/// A [`Command`] for `git` with [`HARDENING_CONFIG`] already applied. Use instead of
/// `Command::new("git")` for anything run against a user-indexed directory.
pub fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for kv in HARDENING_CONFIG {
        cmd.arg("-c").arg(kv);
    }
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
                "-C",
                "/x",
                "status"
            ]
        );
        assert_eq!(cmd.get_program(), "git");
    }
}
