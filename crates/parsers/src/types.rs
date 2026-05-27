use std::path::PathBuf;

/// A semantic chunk extracted from a file — the unit of indexing.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// The file this chunk came from.
    pub source: PathBuf,
    /// Zero-based ordinal within the file (for stable ordering).
    pub seq: usize,
    /// Hierarchical heading breadcrumb, e.g. "Introduction > Background".
    /// Empty for unstructured chunks.
    pub heading: String,
    /// The text content to embed and store in FTS5.
    pub text: String,
    /// Optional language tag for code chunks ("rust", "python", etc.).
    pub language: Option<String>,
}

/// Output of a parser run on one file.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub source: PathBuf,
    pub mime: String,
    pub chunks: Vec<Chunk>,
}

/// Trait implemented by every file parser.
pub trait Parser: Send + Sync {
    /// Returns true if this parser handles the given MIME type.
    fn accepts(&self, mime: &str) -> bool;
    /// Parse the file at `path` and return extracted chunks.
    fn parse(&self, path: &std::path::Path) -> anyhow::Result<Extracted>;
}
