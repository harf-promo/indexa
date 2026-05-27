//! File type parsers: text, Markdown, source code (tree-sitter), PDF, images, audio/video.

pub mod code;
pub mod registry;
pub mod text;
pub mod types;

pub use types::{Chunk, Extracted, Parser};
