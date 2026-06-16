pub mod config;
pub mod decisions;
pub mod fingerprint;
pub mod models_catalog;
pub mod resource;
pub mod smart_classify;
pub mod store;
pub mod surface;
pub mod text;
pub mod walker;
pub mod watcher;

pub use text::{snippet, truncate_chars};
