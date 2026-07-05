//! `GET /api/file?path=` — return a file's raw text (capped) for the in-app preview pane.
//!
//! Security mirrors the MCP `read_file`: the path is canonicalized and must lie within an indexed
//! root (no traversal outside what the user chose to index). Output is capped at ~40 KB; binary
//! files are detected (NUL byte) and return no content. Syntax highlighting is done client-side
//! from the returned `language`, so this stays a plain text + metadata endpoint.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::dto::{err_json, FilePreviewResponse};
use crate::AppState;

/// Match the MCP `read_file` cap so preview and read agree.
const PREVIEW_CAP: usize = 40 * 1024;

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    path: String,
}

/// Coarse language tag from the extension — used by the client highlighter to pick a keyword set.
/// Broader than the indexer's parser set (it's only for display); unknown → `None` (plain text).
fn language_for_ext(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "shell",
        "html" | "htm" => "html",
        "css" => "css",
        "sql" => "sql",
        _ => return None,
    };
    Some(lang)
}

pub(crate) async fn api_file_preview(
    State(state): State<AppState>,
    Query(q): Query<FileQuery>,
) -> Response {
    if q.path.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "missing 'path' query parameter");
    }
    // Canonicalize (resolves symlinks, rejects non-existent paths) before any comparison.
    let requested = match std::fs::canonicalize(&q.path) {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::NOT_FOUND, "path not found"),
    };
    // Path-confinement: must be inside an indexed root (mirrors MCP read_file). Lock the store only
    // to read the roots, then drop it before the filesystem read.
    let roots: Vec<PathBuf> = {
        let store = state.store.lock().await;
        store
            .root_paths()
            .unwrap_or_default()
            .iter()
            .filter_map(|r| std::fs::canonicalize(r).ok())
            .collect()
    };
    if !roots.iter().any(|root| requested.starts_with(root)) {
        return err_json(StatusCode::FORBIDDEN, "path is not within an indexed root");
    }
    let meta = match std::fs::metadata(&requested) {
        Ok(m) => m,
        Err(_) => return err_json(StatusCode::NOT_FOUND, "path not found"),
    };
    if meta.is_dir() {
        return err_json(StatusCode::BAD_REQUEST, "path is a directory, not a file");
    }
    let bytes_total = meta.len();
    let bytes = match std::fs::read(&requested) {
        Ok(b) => b,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read file: {e}"),
            )
        }
    };

    let capped = &bytes[..bytes.len().min(PREVIEW_CAP)];
    // Binary heuristic (shared with the scan walker's filter): a NUL byte in the first 8 KB.
    // Text files don't contain NUL; this avoids dumping garbage from images/binaries.
    let binary = indexa_core::text::is_binary(capped);
    let content = if binary {
        None
    } else {
        // Lossy is fine here: the NUL check already excluded binaries; the only lossy case is a
        // multibyte char split at the 40 KB cap, which renders as a single replacement glyph.
        Some(String::from_utf8_lossy(capped).into_owned())
    };
    let truncated = bytes_total as usize > capped.len();
    let language = language_for_ext(&requested).map(|s| s.to_owned());

    Json(FilePreviewResponse {
        path: q.path,
        language,
        content,
        truncated,
        bytes_total,
        binary,
    })
    .into_response()
}
