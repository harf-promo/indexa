//! `GET /api/file?path=` — return a file's raw text (capped) for the in-app preview pane.
//!
//! Security mirrors the MCP `read_file`: the path is canonicalized and must lie within an indexed
//! root (no traversal outside what the user chose to index) and be an indexed file; secret files
//! and credential stores the indexer keeps out of the index (`.env`, keys, `.pem`, `.ssh/`, …) are
//! refused unless `[scan] include_sensitive`, as is any PEM private key; obvious secrets in the
//! served text are redacted. Only the first ~40 KB is read from disk (bounded read); binary files
//! are detected (NUL byte) and return no content. Syntax highlighting is done client-side from the
//! returned `language`, so this stays a plain text + metadata endpoint.

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
    // Path-confinement: must be inside an indexed root (mirrors MCP read_file) and be an indexed
    // file. Lock the store only for the lookups, then drop it before the filesystem read.
    // Roots are kept as (stored, canonical): entries are recorded under the root as scanned.
    let (roots, indexed) = {
        let store = state.store.lock().await;
        let roots: Vec<(PathBuf, PathBuf)> = store
            .root_paths()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                let stored = PathBuf::from(r);
                std::fs::canonicalize(&stored).ok().map(|c| (stored, c))
            })
            .collect();
        let indexed = is_indexed_file(&store, &q.path, &requested, &roots);
        (roots, indexed)
    };
    let canonical_roots: Vec<PathBuf> = roots.into_iter().map(|(_, c)| c).collect();
    if !canonical_roots
        .iter()
        .any(|root| requested.starts_with(root))
    {
        return err_json(StatusCode::FORBIDDEN, "path is not within an indexed root");
    }
    if requested.is_dir() {
        return err_json(StatusCode::BAD_REQUEST, "path is a directory, not a file");
    }
    if !indexed {
        return err_json(StatusCode::FORBIDDEN, "path is not an indexed file");
    }
    if !state.config.scan.include_sensitive
        && indexa_core::walker::is_sensitive_path(&requested, &canonical_roots)
    {
        return err_json(
            StatusCode::FORBIDDEN,
            "refusing to preview a secret/credential file (set [scan] include_sensitive to allow)",
        );
    }

    let window = match indexa_core::text::read_file_window(&requested, 0, PREVIEW_CAP) {
        Ok(w) => w,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read file: {e}"),
            )
        }
    };
    let bytes_total = window.file_len;
    // Binary heuristic (shared with the scan walker's filter): a NUL byte in the first 8 KB.
    // Text files don't contain NUL; this avoids dumping garbage from images/binaries.
    let binary = indexa_core::text::is_binary(&window.bytes);
    let (content, redacted) = if binary {
        (None, 0)
    } else {
        // The window ends on a char boundary; lossy only matters for genuinely invalid UTF-8.
        let text = String::from_utf8_lossy(&window.bytes);
        if text.contains("PRIVATE KEY-----") {
            return err_json(
                StatusCode::FORBIDDEN,
                "refusing to preview a file containing a private key",
            );
        }
        let (text, n) = indexa_query::redact::redact_secrets(&text);
        (Some(text), n)
    };
    let truncated = window.end < bytes_total;
    let language = language_for_ext(&requested).map(|s| s.to_owned());

    Json(FilePreviewResponse {
        path: q.path,
        language,
        content,
        truncated,
        bytes_total,
        binary,
        redacted,
    })
    .into_response()
}

/// Whether `requested` (canonical) is a `File` row in the index. Looked up as the caller spelled
/// it, as the canonical path, and re-based onto each stored root — entries are stored under the
/// root as it was scanned, which can differ from the canonical form (macOS `/var` vs
/// `/private/var`, a symlinked home).
fn is_indexed_file(
    store: &indexa_core::store::Store,
    raw: &str,
    requested: &Path,
    roots: &[(PathBuf, PathBuf)],
) -> bool {
    let rebased = roots.iter().filter_map(|(stored, canonical)| {
        requested
            .strip_prefix(canonical)
            .ok()
            .map(|rel| stored.join(rel))
    });
    std::iter::once(PathBuf::from(raw))
        .chain(std::iter::once(requested.to_path_buf()))
        .chain(rebased)
        .any(|p| {
            matches!(
                store.entry_by_path(&p.to_string_lossy()),
                Ok(Some(info)) if info.kind == "file"
            )
        })
}
