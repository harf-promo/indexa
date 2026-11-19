//! Path confinement shared by web job and watch handlers.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::path::PathBuf;

use crate::{dto::err_json, AppState};

pub(crate) struct IndexedPathError {
    status: StatusCode,
    message: String,
}

impl IndexedPathError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for IndexedPathError {
    fn into_response(self) -> Response {
        err_json(self.status, self.message)
    }
}

/// Resolve a requested path and keep it within one of the indexed roots.
///
/// Returning the canonical path ensures downstream jobs/watchers use the resolved target rather
/// than re-resolving a caller-controlled `..` or symlink path after validation.
pub(crate) async fn canonical_indexed_path(
    state: &AppState,
    raw: &str,
) -> Result<String, IndexedPathError> {
    if raw.is_empty() || raw.len() > 4096 {
        return Err(IndexedPathError::new(
            StatusCode::BAD_REQUEST,
            "invalid or oversized path",
        ));
    }
    let canonical = std::fs::canonicalize(raw)
        .map_err(|_| IndexedPathError::new(StatusCode::NOT_FOUND, "path not found"))?;
    let roots =
        state.store.lock().await.root_paths().map_err(|e| {
            IndexedPathError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
        })?;
    let inside = roots
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(PathBuf::from(root)).ok())
        .any(|root| canonical.starts_with(root));
    if !inside {
        return Err(IndexedPathError::new(
            StatusCode::FORBIDDEN,
            "path is outside every indexed root",
        ));
    }
    Ok(canonical.to_string_lossy().into_owned())
}
