//! Plugin directory — discovery layer over the compile-time parser "Plugin SDK".
//!
//! [`crate::registry::Registry`]'s doc comment defines the extension mechanism itself
//! (implement [`crate::types::Parser`], call `Registry::register` in your own custom
//! binary). This module is discovery only: a small, human-curated list of known
//! third-party parser crates, so a third-party parser can be found and wired in without
//! re-inventing the snippet each time.
//!
//! The list ships embedded in the binary (`../plugins.toml`, via `include_str!`) and is
//! parsed on each [`load`] call — no network fetch, fully local-first. A future version
//! could fetch a remote curated list instead of (or in addition to) this file; that's
//! explicitly out of scope for now. `indexa plugin list` / `indexa plugin info <name>`
//! (`apps/indexa/src/commands/plugin.rs`) are the CLI surface over this module.

use anyhow::{Context, Result};
use serde::Deserialize;

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

/// Load and parse the embedded plugin directory. Cheap (a few KB of TOML); no caching —
/// call it once per command invocation.
pub fn load() -> Result<Vec<PluginEntry>> {
    let file: DirectoryFile = toml::from_str(PLUGINS_TOML)
        .context("parsing the embedded plugin directory (crates/parsers/plugins.toml)")?;
    Ok(file.plugin)
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
