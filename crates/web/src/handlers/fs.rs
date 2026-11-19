use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::dto::{err_json, FsEntry, PathQuery};

pub(crate) async fn api_fs_ls(Query(params): Query<PathQuery>) -> Response {
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let raw = params
        .path
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| home.to_string_lossy().into_owned());
    match tokio::task::spawn_blocking(move || api_fs_ls_sync(&raw, &home)).await {
        Ok(response) => response,
        Err(error) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("filesystem listing worker failed: {error}"),
        ),
    }
}

fn api_fs_ls_sync(raw: &str, home: &std::path::Path) -> Response {
    // Security: reject path traversal and non-absolute paths.
    let canon = match std::fs::canonicalize(raw) {
        Ok(p) => p,
        Err(_) => return err_json(StatusCode::NOT_FOUND, "path not found"),
    };

    let home_canon = std::fs::canonicalize(home)
        .ok()
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

#[cfg(test)]
mod tests {
    use super::api_fs_ls_sync;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn filesystem_listing_stays_confined_and_only_returns_visible_directories() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::create_dir_all(home.join(".hidden")).unwrap();
        std::fs::write(home.join("notes.txt"), "not a directory").unwrap();

        let response = api_fs_ls_sync(home.to_str().unwrap(), &home);
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "projects");

        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let response = api_fs_ls_sync(outside.to_str().unwrap(), &home);
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }
}
