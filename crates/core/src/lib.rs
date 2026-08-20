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
pub mod surface;
pub mod text;
pub mod walker;
pub mod watcher;

pub use text::{snippet, truncate_chars};
