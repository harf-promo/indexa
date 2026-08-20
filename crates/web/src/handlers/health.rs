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

/// Whether the "hierarchical context is thin" banner should fire, given the real
/// DIRECTORY coverage from `Store::coverage_stats` (`built` dirs out of `total_dirs`
/// — the same figures `/api/map` renders as "N of M folders").
///
/// Pure (takes the counts, not a `Store`) so it's unit-testable directly. Deliberately
/// NOT based on `summary_count`/`entry_count`: those run an unfiltered `COUNT(*)` over
/// `summaries`/`entries`, and both tables hold file AND dir rows side by side, so the
/// result blends the two. A repo with excellent file coverage but zero folder roll-ups
/// used to read as "covered" even though folder overviews/Export — what this banner
/// promises — were 0% built. Concretely: 800/1000 files summarized + 6/130 dirs rolled
/// up used to blend to 806/1130 ≈ 71% ("not thin"), while the real folder coverage was
/// 6/130 ≈ 5% (should be thin). This function reports the latter.
///
/// Threshold stays 10%, now applied to the true (much smaller) directory population:
/// it still requires real folder-level progress before the banner clears, and — unlike
/// the old blend — it can never read "not thin" while folder coverage sits at 0%.
/// `total_dirs == 0` only happens for a genuinely empty index (no root scanned yet):
/// the walker (`walker.rs`) always records the scanned root itself as a `kind = 'dir'`
/// entry, even for a single flat folder with no subdirectories, so any non-empty index
/// has `total_dirs >= 1`.
fn is_thin_context(total_dirs: u64, built_dirs: u64) -> bool {
    const THIN_RATIO: f64 = 0.10;
    total_dirs > 0 && (built_dirs as f64 / total_dirs as f64) < THIN_RATIO
}

pub(crate) async fn api_health(State(state): State<AppState>) -> Response {
    let (entries, chunks, summaries, last, dir_coverage) = {
        let store = state.store.lock().await;
        (
            store.entry_count().unwrap_or(0),
            store.chunk_count().unwrap_or(0),
            store.summary_count().unwrap_or(0),
            store.last_indexed_at().ok().flatten(),
            store.coverage_stats().ok(),
        )
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_days = last.map(|ts| ((now - ts) / 86_400).max(0));
    let stale = age_days.is_some_and(|d| d >= STALE_AFTER_DAYS);
    // Distinct from `stale` (mtime) — see `is_thin_context` for why this is DIRECTORY
    // coverage (matching `/api/map`), not the blended `summaries`/`entries` above.
    let (total_dirs, built_dirs) = dir_coverage
        .map(|(total_dirs, built, ..)| (total_dirs, built))
        .unwrap_or((0, 0));
    let thin_context = is_thin_context(total_dirs, built_dirs);
    // Best-effort, desktop-only signal — `null` under plain `indexa serve`.
    let cli_skew = indexa_core::config::default_data_dir()
        .as_deref()
        .and_then(read_cli_skew_marker);
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "entries": entries,
        "chunks": chunks,
        "summaries": summaries,
        "index_age_days": age_days,
        "stale": stale,
        "thin_context": thin_context,
        "cli_skew": cli_skew,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{is_thin_context, read_cli_skew_marker};
    use indexa_core::store::Store;
    use indexa_core::walker::{Entry, EntryKind};
    use std::path::PathBuf;

    fn entry(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size: 10,
            modified: None,
            hint: None,
            is_binary: false,
        }
    }

    /// Against a REAL `Store` (not hand-picked numbers): seeds 4 dirs / 2 files, marks
    /// exactly 1 dir "done", and pins the full `coverage_stats()` tuple shape
    /// `(total_dirs, built, partial, failed, none, total_chunks, total_files)` — the
    /// same destructure `api_health` relies on (`.map(|(total_dirs, built, ..)| ...)`).
    /// A future reordering of that tuple would fail here before it could silently feed
    /// the wrong fields into `is_thin_context`.
    #[test]
    fn coverage_stats_positions_match_a_seeded_store() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                entry("/r", EntryKind::Dir),
                entry("/r/a", EntryKind::Dir),
                entry("/r/b", EntryKind::Dir),
                entry("/r/c", EntryKind::Dir),
                entry("/r/f1.txt", EntryKind::File),
                entry("/r/a/f2.txt", EntryKind::File),
            ])
            .unwrap();
        store
            .enqueue_summary_items(&[
                ("/r".to_owned(), "dir".to_owned(), 0),
                ("/r/a".to_owned(), "dir".to_owned(), 1),
                ("/r/b".to_owned(), "dir".to_owned(), 1),
                ("/r/c".to_owned(), "dir".to_owned(), 1),
            ])
            .unwrap();
        store.mark_queue_state("/r/a", "done", None).unwrap();

        let stats = store.coverage_stats().unwrap();
        assert_eq!(
            stats,
            (4, 1, 3, 0, 0, 0, 2),
            "(total_dirs, built, partial, failed, none, total_chunks, total_files)"
        );

        let (total_dirs, built) = (stats.0, stats.1);
        assert_eq!((total_dirs, built), (4, 1));
        // 1 of 4 dirs built = 25%, above the 10% threshold — not thin.
        assert!(!is_thin_context(total_dirs, built));
    }

    #[test]
    fn no_directories_is_never_thin() {
        // `total_dirs == 0` only happens for a genuinely empty index — no root scanned
        // yet. The walker always records the scanned root itself as a `kind = 'dir'`
        // entry (even a single flat folder with no subdirectories), so any non-empty
        // index has `total_dirs >= 1`; this is not a live "flat repo" case.
        assert!(!is_thin_context(0, 0));
    }

    #[test]
    fn zero_built_dirs_is_thin() {
        // The failure mode the old blended `summaries`/`entries` ratio allowed: 0%
        // real folder coverage must always read as thin, however many FILE summaries
        // exist elsewhere (this function only sees the directory counts).
        assert!(is_thin_context(1, 0));
        assert!(is_thin_context(2_507, 0));
    }

    #[test]
    fn below_threshold_is_thin() {
        // 6 of 130 dirs ≈ 4.6% — well under 10%, and (unlike the old blend) this
        // stays thin even if file coverage in the same repo is 80%+.
        assert!(is_thin_context(130, 6));
    }

    #[test]
    fn at_or_above_threshold_is_not_thin() {
        // Exactly 10% is NOT thin (`< THIN_RATIO`, not `<=`) — pin the boundary.
        assert!(!is_thin_context(100, 10));
        assert!(!is_thin_context(100, 50));
        assert!(!is_thin_context(20, 20));
    }

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
