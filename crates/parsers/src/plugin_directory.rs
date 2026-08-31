//! Plugin directory — discovery layer over the compile-time parser "Plugin SDK".
//!
//! [`crate::registry::Registry`]'s doc comment defines the extension mechanism itself
//! (implement [`crate::types::Parser`], call `Registry::register` in your own custom
//! binary). This module is discovery only: a small, human-curated list of known
//! third-party parser crates, so a third-party parser can be found and wired in without
//! re-inventing the snippet each time.
//!
//! The list ships embedded in the binary (`../plugins.toml`, via `include_str!`) and is
//! parsed on each [`load`] call — fully local-first, no network fetch required. [`load_remote`]
//! additionally fetches the same file straight off `main` on GitHub, so a user can see
//! newly-curated entries without upgrading their `indexa` binary — `indexa plugin list
//! --refresh` (`apps/indexa/src/commands/plugin.rs`) is the CLI surface for that, and it
//! always fails open to [`load`] on any network/parse error.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Raw `plugins.toml` off `main`, mirroring the embedded copy but always current.
const REMOTE_DIRECTORY_URL: &str =
    "https://raw.githubusercontent.com/harf-promo/indexa/main/crates/parsers/plugins.toml";

/// Same `User-Agent` convention as `crates/update`'s GitHub client.
const USER_AGENT: &str = concat!("indexa/", env!("CARGO_PKG_VERSION"));

/// One entry in the plugin directory: everything needed to find, evaluate, and wire in
/// a third-party parser crate.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginEntry {
    /// Short, unique, kebab-case handle — what `indexa plugin info <name>` matches.
    pub name: String,
    /// The crate's name on crates.io (may contain dashes).
    pub crate_name: String,
    /// The Rust type implementing `Parser` that callers should `Box::new(...)` when
    /// registering — see [`crate::registry::Registry::register`].
    pub parser_type: String,
    /// One-sentence description of what the parser handles.
    pub description: String,
    /// File extensions it handles, without the leading dot (e.g. `["mydata"]`).
    #[serde(default)]
    pub extensions: Vec<String>,
    /// MIME types it handles, when extensions aren't the natural fit.
    #[serde(default)]
    pub mime_types: Vec<String>,
    /// Source repository URL.
    pub repo: String,
}

impl PluginEntry {
    /// One-line summary of what this plugin handles, for the `plugin list` table:
    /// extensions if any (dot-prefixed), else MIME types, else an em dash.
    pub fn handles(&self) -> String {
        if !self.extensions.is_empty() {
            self.extensions
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join(", ")
        } else if !self.mime_types.is_empty() {
            self.mime_types.join(", ")
        } else {
            "—".to_string()
        }
    }

    /// The crate name as a valid Rust module/ident path (crates.io names may use dashes;
    /// `use` paths need underscores).
    pub fn crate_ident(&self) -> String {
        self.crate_name.replace('-', "_")
    }
}

/// TOML shape of `plugins.toml`: an array of `[[plugin]]` tables.
#[derive(Debug, Default, Deserialize)]
struct DirectoryFile {
    #[serde(default, rename = "plugin")]
    plugin: Vec<PluginEntry>,
}

/// The curated directory data, embedded at compile time — hand-edited, not generated.
const PLUGINS_TOML: &str = include_str!("../plugins.toml");

/// Parse `plugins.toml`'s shape (`[[plugin]] ...`) out of raw TOML text — shared by
/// [`load`] (the embedded copy) and [`load_remote`] (the fetched one), so the two never
/// drift apart.
fn parse_directory_toml(raw: &str) -> Result<Vec<PluginEntry>> {
    let file: DirectoryFile = toml::from_str(raw).context("parsing the plugin directory TOML")?;
    Ok(file.plugin)
}

/// Load and parse the embedded plugin directory. Cheap (a few KB of TOML); no caching —
/// call it once per command invocation.
pub fn load() -> Result<Vec<PluginEntry>> {
    parse_directory_toml(PLUGINS_TOML)
        .context("parsing the embedded plugin directory (crates/parsers/plugins.toml)")
}

/// Build the `reqwest::Client` used for [`load_remote`]: rustls-only (the workspace's
/// openssl-free-tree invariant, inherited from `indexa-http-util`'s own feature set),
/// the same `User-Agent` convention as `crates/update`'s GitHub client, a short connect
/// timeout (10s, `indexa-http-util`'s fixed default) and a modest whole-request timeout
/// (15s) — this is a small text file, not a release binary, so it should fail fast
/// rather than hang.
///
/// Stays `Result`-returning for API stability with `cmd_plugin_list`'s fallback wiring
/// (`apps/indexa/src/commands/plugin.rs`), but — like every other `indexa-http-util`-
/// backed client in the codebase (`http_client`, `ssrf_guarded_client`, and every
/// LLM/embed provider adapter) — `build()`'s one documented failure mode (unrecoverable
/// rustls TLS/OS-trust-store init) now panics via `expect` inside
/// `http_client_with_user_agent` rather than surfacing as an `Err` here. Narrows, but
/// doesn't remove, `indexa plugin list --refresh`'s embedded-directory fallback: a
/// fetch or parse failure still falls back cleanly (`cmd_plugin_list`'s `match` on
/// [`load_remote`]'s `Result`), only a TLS-init failure at construction time no longer
/// does.
pub fn build_remote_client() -> Result<reqwest::Client> {
    Ok(indexa_http_util::http_client_with_user_agent(
        15, USER_AGENT,
    ))
}

/// Fetch and parse `plugins.toml` off `main` on GitHub — the same curated list as
/// [`load`], but reflecting whatever's currently committed upstream rather than what
/// shipped with this binary. No caching; callers decide how to handle failure (this
/// module never falls back on its own — see `cmd_plugin_list`'s `--refresh` handling).
pub async fn load_remote(client: &reqwest::Client) -> Result<Vec<PluginEntry>> {
    let raw = client
        .get(REMOTE_DIRECTORY_URL)
        .send()
        .await
        .context("fetching the remote plugin directory")?
        .error_for_status()
        .context("remote plugin directory request failed")?
        .text()
        .await
        .context("reading the remote plugin directory response body")?;
    parse_directory_toml(&raw).context("parsing the remote plugin directory")
}

/// Find one entry by name, case-insensitive.
pub fn find(name: &str) -> Result<Option<PluginEntry>> {
    Ok(load()?
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_directory_parses() {
        // The shipped plugins.toml must always parse — a curator typo here would
        // otherwise only surface at `indexa plugin list` runtime, not at build/test time.
        let entries = load().expect("plugins.toml must parse");
        // The seeded template entry (or a real one, once added) should be present.
        assert!(!entries.is_empty());
    }

    #[test]
    fn find_is_case_insensitive() {
        let entries = load().unwrap();
        let name = entries[0].name.to_ascii_uppercase();
        assert!(find(&name).unwrap().is_some());
    }

    #[test]
    fn find_missing_returns_none() {
        assert!(find("definitely-not-a-real-plugin-xyz").unwrap().is_none());
    }

    #[test]
    fn handles_prefers_extensions_over_mime_types() {
        let p = PluginEntry {
            name: "t".into(),
            crate_name: "t".into(),
            parser_type: "T".into(),
            description: "d".into(),
            extensions: vec!["foo".into(), "bar".into()],
            mime_types: vec!["application/x-foo".into()],
            repo: "https://example.com".into(),
        };
        assert_eq!(p.handles(), ".foo, .bar");
    }

    #[test]
    fn handles_falls_back_to_mime_types_then_dash() {
        let mut p = PluginEntry {
            name: "t".into(),
            crate_name: "t".into(),
            parser_type: "T".into(),
            description: "d".into(),
            extensions: vec![],
            mime_types: vec!["application/x-foo".into()],
            repo: "https://example.com".into(),
        };
        assert_eq!(p.handles(), "application/x-foo");
        p.mime_types.clear();
        assert_eq!(p.handles(), "—");
    }

    #[test]
    fn parse_directory_toml_parses_a_crafted_entry() {
        let raw = r#"
[[plugin]]
name = "crafted"
crate_name = "indexa-parser-crafted"
parser_type = "CraftedParser"
description = "A hand-crafted test entry."
extensions = ["crf"]
mime_types = []
repo = "https://example.com/crafted"
"#;
        let entries = parse_directory_toml(raw).expect("valid TOML must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "crafted");
        assert_eq!(entries[0].handles(), ".crf");
    }

    #[test]
    fn parse_directory_toml_empty_string_is_an_empty_list() {
        // No `[[plugin]]` tables at all is valid TOML (an empty directory), not an error.
        let entries = parse_directory_toml("").expect("empty input is a valid, empty directory");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_directory_toml_rejects_malformed_toml() {
        let malformed = "this is not [[valid toml at all";
        assert!(parse_directory_toml(malformed).is_err());
    }

    #[test]
    fn parse_directory_toml_rejects_a_plugin_missing_a_required_field() {
        // `repo` is required (no `#[serde(default)]`) — a curator dropping it should error
        // cleanly at parse time, not panic or silently substitute an empty string.
        let raw = r#"
[[plugin]]
name = "incomplete"
crate_name = "indexa-parser-incomplete"
parser_type = "IncompleteParser"
description = "Missing the repo field."
"#;
        assert!(parse_directory_toml(raw).is_err());
    }

    #[test]
    fn crate_ident_replaces_dashes() {
        let mut p = PluginEntry {
            name: "t".into(),
            crate_name: "indexa-parser-example".into(),
            parser_type: "T".into(),
            description: "d".into(),
            extensions: vec![],
            mime_types: vec![],
            repo: "https://example.com".into(),
        };
        assert_eq!(p.crate_ident(), "indexa_parser_example");
        p.crate_name = "already_underscored".into();
        assert_eq!(p.crate_ident(), "already_underscored");
    }
}
