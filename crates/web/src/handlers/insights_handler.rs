//! Insights REST API (v0.10) + the insights→ledger bridge (v0.24).
//!
//! Routes:
//!   GET  /api/insights/duplicates?threshold=0.95&exact=false
//!   GET  /api/insights/stale?days=365
//!   GET  /api/insights/diff?days=7
//!   POST /api/review/dismiss-evidence  — "don't ask about this" from insights

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::decisions::detectors;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dto::err_json;
use crate::AppState;

#[derive(Deserialize)]
pub(crate) struct DuplicatesQuery {
    threshold: Option<f32>,
    exact: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct StaleQuery {
    days: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct DiffQuery {
    days: Option<i64>,
}

pub(crate) async fn api_insights_duplicates(
    State(state): State<AppState>,
    Query(q): Query<DuplicatesQuery>,
) -> Response {
    let threshold = q.threshold.unwrap_or(0.95).clamp(0.0, 1.0);
    let exact = q.exact.unwrap_or(false);

    // Duplicate detection (esp. the O(n²) near-dup scan) can take seconds-to-minutes
    // on a large index. Run it on a fresh, short-lived Store connection inside
    // spawn_blocking so it never holds the shared Store mutex (which would block every
    // other API request) and never stalls the async runtime.
    let db_path = state.db_path.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let store = indexa_core::store::Store::open(&db_path)?;
        if exact {
            store.find_exact_duplicates()
        } else {
            store.find_near_duplicates(threshold)
        }
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("duplicate-scan task panicked: {e}")));

    match result {
        Ok(clusters) => {
            let items: Vec<serde_json::Value> = clusters
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "paths": c.paths,
                        "similarity": c.similarity,
                        "exact": c.exact,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "threshold": threshold,
                "exact": exact,
                "clusters": items,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_insights_stale(
    State(state): State<AppState>,
    Query(q): Query<StaleQuery>,
) -> Response {
    let days = q.days.unwrap_or(365).max(1);
    let store = state.store.lock().await;
    match store.find_stale_entries(days) {
        Ok(entries) => {
            let items: Vec<serde_json::Value> = entries
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "path": e.path,
                        "kind": e.kind,
                        "modified_s": e.modified_s,
                        "days_since_modified": e.days_since_modified,
                    })
                })
                .collect();
            Json(serde_json::json!({ "days": days, "entries": items })).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_insights_diff(
    State(state): State<AppState>,
    Query(q): Query<DiffQuery>,
) -> Response {
    let days = q.days.unwrap_or(7).max(1);
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - days * 86_400)
        .unwrap_or(0);

    let store = state.store.lock().await;
    match store.weekly_diff(since) {
        Ok(diff) => Json(serde_json::json!({
            "days": days,
            "added": diff.added,
            "modified": diff.modified,
            "added_count": diff.added_count,
            "modified_count": diff.modified_count,
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub(crate) struct DismissEvidenceBody {
    /// `"duplicate"` (one cluster per call) or `"archive"` (one or more dirs).
    kind: String,
    paths: Vec<String>,
}

/// "Don't ask about this" from the insights tab: record the question the
/// detector would raise as an already-DISMISSED ledger row, so sticky
/// dismissal suppresses it before it ever reaches the inbox. Returns how many
/// dismissal rows now stand (archive paths without an indexed mtime are
/// skipped — the detector never asks about those anyway).
pub(crate) async fn api_review_dismiss_evidence(
    State(state): State<AppState>,
    Json(body): Json<DismissEvidenceBody>,
) -> Response {
    if body.paths.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "paths must not be empty");
    }
    let mut store = state.store.lock().await;
    let result: anyhow::Result<usize> = match body.kind.as_str() {
        "duplicate" => {
            if body.paths.len() < 2 {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "a duplicate cluster needs at least 2 paths",
                );
            }
            // The server re-derives the cluster from the detector's own scan —
            // client-echoed threshold/exact flags can never byte-match the
            // detector's evidence fingerprint, which is what stickiness keys on.
            detectors::predismiss_duplicate(&mut store, &body.paths).map(usize::from)
        }
        "archive" => body.paths.iter().try_fold(0usize, |n, p| {
            Ok(n + usize::from(detectors::predismiss_archive(&mut store, p)?))
        }),
        other => {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("unknown evidence kind '{other}' (known: duplicate, archive)"),
            )
        }
    };
    match result {
        Ok(n) => Json(serde_json::json!({ "dismissed": n })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
