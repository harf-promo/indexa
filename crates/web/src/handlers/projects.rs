//! `GET /api/projects` — top-level detected apps with coverage, for the
//! welcome "Build context" list. No schema change; reads `directory_apps`
//! plus the same coverage rollup the tree uses.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use indexa_core::store::Store;

use crate::dto::{err_json, file_name_of, PathQuery, ProjectResponse};
use crate::AppState;

pub(crate) async fn api_projects(
    State(state): State<AppState>,
    Query(params): Query<PathQuery>,
) -> Response {
    let prefix = params.path.as_deref().unwrap_or("");
    // Open a fresh, short-lived read connection instead of locking the shared store for
    // the whole per-project coverage loop (3 subtree scans each, ~195ms/16 projects
    // measured live) — mirrors tree.rs's api_tree so a slow projects list no longer
    // serializes every other web request that needs the store.
    let store = match Store::open(&state.db_path) {
        Ok(s) => s,
        Err(e) => {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
    };
    match projects_from(&store, prefix) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn projects_from(
    store: &indexa_core::store::Store,
    prefix: &str,
) -> anyhow::Result<Vec<ProjectResponse>> {
    let apps = store.top_projects_under(prefix)?;
    let mut out = Vec::with_capacity(apps.len());
    for app in apps {
        let cov = store.path_coverage(&app.path)?;
        out.push(ProjectResponse {
            name: project_display_name(&app.path),
            path: app.path,
            app_name: app.app_name,
            chunk_count: cov.chunk_count,
            has_summary: cov.has_summary,
            covered: cov.covered,
            total: cov.total,
        });
    }
    Ok(out)
}

/// Basename, plus the parent folder when the basename is a generic
/// `admin`/`mobile`/`app` so Barrq/admin and Barrq/mobile stay distinct.
fn project_display_name(path: &str) -> String {
    const GENERIC: &[&str] = &[
        "admin", "mobile", "app", "ios", "android", "system", "web", "api", "frontend", "backend",
        "server", "client", "src",
    ];
    let name = file_name_of(path);
    if GENERIC.iter().any(|g| name.eq_ignore_ascii_case(g)) {
        if let Some(parent) = std::path::Path::new(path)
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
        {
            return format!("{parent}/{name}");
        }
    }
    name
}
