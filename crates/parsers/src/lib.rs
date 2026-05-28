//! File type parsers: text, Markdown, source code (tree-sitter), images, audio/video, office.

pub mod code;
pub mod epub;
pub mod image;
pub mod media;
pub mod office;
pub mod org;
pub mod pdf;
pub mod registry;
pub mod text;
pub mod types;

pub use types::{Chunk, Extracted, Parser};
