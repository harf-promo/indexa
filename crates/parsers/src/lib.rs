//! File type parsers: text, Markdown, source code (tree-sitter), images, audio/video, office.

pub mod archive;
pub mod code;
pub mod email;
pub mod epub;
pub mod html;
pub mod image;
pub mod ipynb;
pub mod media;
pub mod office;
pub mod org;
pub mod pdf;
pub mod presentation;
pub mod registry;
pub mod svg;
pub mod text;
pub mod types;

pub use registry::Registry;
pub use types::{Chunk, Extracted, Parser};
