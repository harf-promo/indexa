//! `GET /api/health` — server version + index freshness (v0.39).
//!
//! Surfaces the two things that silently rotted before: the running binary's
//! version (so a stale CLI/MCP/app is visible) and how long ago the index was
//! last updated (so answers built on a stale snapshot are flagged). No network,
//! no secrets — a cheap read the UI polls on load to show a staleness banner.

use std::path::Path;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};

use crate::AppState;

/// Index is considered stale once its newest content is older than this. A week
/// is conservative: long enough not to nag during active work, short enough that
/// "answers may be out of date" is honest.
const STALE_AFTER_DAYS: i64 = 7;

/// Read the desktop-written CLI-skew marker, if present, from `data_dir`.
///
/// The desktop app writes `<data_dir>/cli_skew_warning.json` after an app update
/// whose CLI auto-refresh did NOT land the expected version (and deletes it on
/// success), so the web UI can surface "your terminal/MCP `indexa` is stale". Pure
/// (takes the dir explicitly) so it can be unit-tested without the real data dir.
/// Fail-open: any missing file / parse error → `None`.
pub(crate) fn read_cli_skew_marker(data_dir: &Path) -> Option<serde_json::Value> {
    let path = data_dir.join(indexa_update::CLI_SKEW_MARKER_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Only surface a JSON object — a stray scalar/array marker would otherwise render
    // a content-less banner. The desktop writer always emits an object.
    value.is_object().then_some(value)
}

pub(crate) async fn api_health(State(state): State<AppState>) -> Response {
    let (entries, chunks, last) = {
        let store = state.store.lock().await;
        (
            store.entry_count().unwrap_or(0),
            store.chunk_count().unwrap_or(0),
            store.last_indexed_at().ok().flatten(),
        )
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_days = last.map(|ts| ((now - ts) / 86_400).max(0));
    let stale = age_days.is_some_and(|d| d >= STALE_AFTER_DAYS);
    // Best-effort, desktop-only signal — `null` under plain `indexa serve`.
    let cli_skew = indexa_core::config::default_data_dir()
        .as_deref()
        .and_then(read_cli_skew_marker);
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "entries": entries,
        "chunks": chunks,
        "index_age_days": age_days,
        "stale": stale,
        "cli_skew": cli_skew,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::read_cli_skew_marker;
    use std::path::PathBuf;

    // Unique temp dir per test (no `tempfile` dep — mirrors lib.rs `temp_db_path`).
    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("indexa-skew-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn marker_round_trips_when_present() {
        let dir = temp_dir("present");
        std::fs::write(
            dir.join(indexa_update::CLI_SKEW_MARKER_FILE),
            r#"{"app_version":"0.65.0","cli_version":"0.51.0","cli_path":"/x/indexa"}"#,
        )
        .unwrap();
        let v = read_cli_skew_marker(&dir).expect("marker present");
        assert_eq!(v["app_version"], "0.65.0");
        assert_eq!(v["cli_version"], "0.51.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_marker_is_none() {
        let dir = temp_dir("absent");
        assert!(read_cli_skew_marker(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_marker_is_none() {
        let dir = temp_dir("malformed");
        std::fs::write(dir.join(indexa_update::CLI_SKEW_MARKER_FILE), "not json{").unwrap();
        assert!(read_cli_skew_marker(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_object_marker_is_none() {
        // Valid JSON but not an object → no content-less banner.
        let dir = temp_dir("nonobject");
        std::fs::write(dir.join(indexa_update::CLI_SKEW_MARKER_FILE), "[1,2,3]").unwrap();
        assert!(read_cli_skew_marker(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
