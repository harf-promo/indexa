use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{err_json, FsEntry, PathQuery};

pub(crate) async fn api_fs_ls(Query(params): Query<PathQuery>) -> Response {
    let raw = match params.path.as_deref() {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => directories::BaseDirs::new()
            .map(|b| b.home_dir().to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_owned()),
    };

    // Security: reject path traversal and non-absolute paths.
    let canon = match std::fs::canonicalize(&raw) {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::NOT_FOUND, "path not found"),
    };

    let home_canon = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .and_then(|h| std::fs::canonicalize(h).ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));

    // Clamp to HOME to prevent exposing system dirs.
    if !canon.starts_with(&home_canon) {
        return err_json(StatusCode::FORBIDDEN, "path outside home directory");
    }

    let mut entries: Vec<FsEntry> = Vec::new();

    // Add parent dir navigation (as long as we're not already at home).
    if canon != home_canon {
        if let Some(parent) = canon.parent() {
            entries.push(FsEntry {
                name: "..".into(),
                path: parent.to_string_lossy().into_owned(),
            });
        }
    }

    let rd = match std::fs::read_dir(&canon) {
        Ok(rd) => rd,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let mut dirs: Vec<FsEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && !e.file_name().to_string_lossy().starts_with('.')
        })
        .map(|e| FsEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path().to_string_lossy().into_owned(),
        })
        .collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    entries.extend(dirs);

    Json(entries).into_response()
}
