//! `indexa-core` — the foundation crate at the bottom of the workspace DAG.
//!
//! Owns the on-disk index and everything that does not depend on a surface (CLI / web /
//! MCP) or a model adapter. Other crates build up from here; nothing here depends on them.
//!
//! Key modules:
//! - [`store`] — the SQLite index: one file per concern (entries, chunks, FTS, summaries,
//!   edges, weights, decisions, …), schema/migrations in `store::schema`. The single
//!   source of truth for indexed data.
//! - [`walker`] — filesystem traversal producing [`walker::Entry`] records.
//! - [`watcher`] — incremental re-index on file-change events.
//! - [`config`] — the user config model (`indexa.toml`) and defaults.
//! - [`resource`] — the memory-budget watchdog (keys on available, not total−used memory).
//! - [`smart_classify`] / [`decisions`] — file classification and the human-judgment ledger.
//! - [`text`] / [`fingerprint`] / [`surface`] — shared text utilities and content hashing.
//!
//! See `docs/architecture.md` for how this crate fits the wider system and the
//! "where to add things" contributor map.

pub mod app_detect;
pub mod cochange;
pub mod config;
pub mod decisions;
pub mod fingerprint;
pub mod gitdiff;
pub mod models_catalog;
pub mod notes;
pub mod pathutil;
pub mod resource;
pub mod smart_classify;
pub mod store;
pub mod summary_drift;
pub mod surface;
pub mod text;
pub mod walker;
pub mod watcher;

pub use text::{snippet, truncate_chars};

#[cfg(test)]
mod doc_freshness {
    /// `docs/COMPETITIVE.md` carries a "Snapshot updated <date> (vX.Y.Z)" stamp and tells the
    /// reader that CHANGELOG.md is canonical for anything shipped after it. That contract only
    /// holds if the stamp is roughly current — it had drifted to v0.77.0 against a v0.80.3
    /// tree, three minor releases of shipped features the page didn't know about.
    ///
    /// Asserted at MINOR precision, deliberately: a patch release shouldn't force a
    /// competitive re-read, but a minor bump means features shipped and the page is worth
    /// re-checking. Reads the file at test time rather than `include_str!`-ing it, so editing
    /// a doc doesn't recompile the crate — same idiom as `doc_tool_count_matches_code`.
    #[test]
    fn competitive_snapshot_stamp_tracks_the_workspace_minor_version() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = std::fs::read_to_string(repo.join("docs/COMPETITIVE.md")).unwrap();

        let marker = "Snapshot updated ";
        let at = text
            .find(marker)
            .expect("docs/COMPETITIVE.md must carry a 'Snapshot updated <date> (vX.Y.Z)' stamp");
        let open = text[at..]
            .find("(v")
            .expect("the snapshot stamp must name a version as (vX.Y.Z)")
            + at
            + 2;
        let close = text[open..]
            .find(')')
            .expect("unterminated version in the snapshot stamp")
            + open;
        let stamped = &text[open..close];

        let minor = |v: &str| {
            let mut it = v.split('.');
            let major = it.next().unwrap_or_default().to_owned();
            let minor = it.next().unwrap_or_default().to_owned();
            format!("{major}.{minor}")
        };
        let want = minor(env!("CARGO_PKG_VERSION"));
        assert_eq!(
            minor(stamped),
            want,
            "docs/COMPETITIVE.md is stamped v{stamped} but the workspace is at v{} — re-read the \
             page against what has shipped since, then update the stamp (a patch bump alone \
             would not have tripped this)",
            env!("CARGO_PKG_VERSION")
        );
    }
}
