use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{generate, Shell};
use indexa_cli::Cli;

/// Print a shell-completion script for `shell` to stdout.
///
/// The script is generated from the live clap definition, so it always reflects
/// every current subcommand and flag — there is no hand-maintained completion
/// file to drift out of sync as the CLI grows.
pub(crate) fn cmd_completion(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "indexa", &mut std::io::stdout());
    Ok(())
}
