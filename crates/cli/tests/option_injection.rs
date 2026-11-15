//! `indexa index` is spawned by the MCP `trigger_index` tool with a caller-supplied path.
//! These tests pin the clap behaviour that makes `--` mandatory there: without it a path
//! such as `--config=<file>` is parsed as the global config flag, not as a path.

use clap::Parser;
use indexa_cli::{Cli, Commands};

#[test]
fn option_shaped_path_is_a_flag_without_the_separator() {
    let cli = Cli::try_parse_from(["indexa", "index", "--config=/tmp/x.toml"]).unwrap();
    assert_eq!(cli.config.as_deref(), Some("/tmp/x.toml"));
    match cli.command {
        Commands::Index { paths, .. } => assert!(paths.is_empty()),
        _ => panic!("wrong command"),
    }
}

#[test]
fn option_shaped_path_after_the_separator_is_just_a_path() {
    let cli = Cli::try_parse_from(["indexa", "index", "--", "--config=/tmp/x.toml"]).unwrap();
    assert_eq!(
        cli.config, None,
        "must not be read as the global --config flag"
    );
    match cli.command {
        Commands::Index { paths, .. } => assert_eq!(paths, vec!["--config=/tmp/x.toml"]),
        _ => panic!("wrong command"),
    }
}
