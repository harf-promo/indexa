use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::dto::{err_json, require_path, PathQuery};
use crate::AppState;

pub(crate) async fn api_delete_entry(
    Query(q): Query<PathQuery>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let path = match require_path(q) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // require_path accepts an empty string; guard the one destructive endpoint so an empty
    // (or whitespace) path can't reach delete_subtree.
    if path.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "path must not be empty");
    }
    let mut store = s.store.lock().await;
    match store.delete_subtree(&path) {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_version() -> impl IntoResponse {
    Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }))
}

/// Return the last N lines of today's log file (for error reports).
pub(crate) async fn api_logs_tail(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let lines: usize = params
        .get("lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(500);

    // tracing-appender rolling::daily creates files named "prefix.YYYY-MM-DD".
    // Pick the most recently modified log file under the log dir.
    let log_dir = &*state.log_dir;
    let candidates: Vec<_> = std::fs::read_dir(log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("indexa.log"))
        .collect();

    // Pick the most recently modified log file.
    let best = candidates
        .iter()
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    // Read a bounded window from the END of the (possibly huge) day's log rather than the whole
    // file just to return the last `lines`.
    let content = match best {
        Some(entry) => read_tail_window(&entry.path(), lines),
        None => String::new(),
    };

    let tail: String = content
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    Json(serde_json::json!({ "lines": tail }))
}

/// Read the last window of a file — enough for `lines` lines at a generous ~512 bytes/line,
/// clamped to [4 KiB, 256 KiB] — by seeking from EOF instead of loading the whole file. Fails
/// open to an empty string.
///
/// UTF-8 safety: a byte in `0x80..=0xBF` is always a UTF-8 *continuation* byte — never a valid
/// character start — so if the seek landed mid-character, the read buffer opens with 1-3 of
/// them. We skip forward past those (never backward: re-reading earlier bytes would defeat the
/// point of a bounded tail read) before the lossy UTF-8 conversion, so a boundary the seek
/// itself introduced never turns into a `U+FFFD` replacement glyph in the output. We then also
/// drop the leading (now-guaranteed-complete-as-UTF-8, but still possibly truncated-as-a-line)
/// partial line, since starting mid-file almost always starts mid-line too.
///
/// `crates/core/src/text.rs`'s `floor_char_boundary` is not reusable here: it walks *backward*
/// from an already-valid `&str` to find a safe slice point, which fits truncating a decoded
/// string from the front. Here we need to walk *forward* over raw, not-yet-decoded bytes read
/// from an arbitrary seek offset — a different direction over a different input, so the skip
/// loop below is written directly instead.
fn read_tail_window(path: &std::path::Path, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let window = (lines.saturating_mul(512)).clamp(4096, 256 * 1024) as u64;
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(window);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let mut skip = 0;
    while skip < buf.len() && skip < 3 && (buf[skip] & 0xC0) == 0x80 {
        skip += 1;
    }
    let mut s = String::from_utf8_lossy(&buf[skip..]).into_owned();
    // Drop the leading partial line when the window started mid-file.
    if start > 0 {
        if let Some(nl) = s.find('\n') {
            s.drain(..=nl);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(tag: &str, content: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "indexa-web-test-tail-{}-{tag}.log",
            std::process::id()
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn read_tail_window_returns_full_content_when_smaller_than_window() {
        let p = write_temp("small", b"line one\nline two\nline three\n");
        let out = read_tail_window(&p, 50);
        assert_eq!(out, "line one\nline two\nline three\n");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_tail_window_drops_leading_partial_line_on_a_large_file() {
        // Force a tiny window (lines=1 -> clamped to the 4 KiB floor) against a file bigger than
        // that floor, so the read genuinely starts mid-file, not at byte 0.
        let mut content = String::new();
        for i in 0..2000 {
            content.push_str(&format!("line-{i:04}\n"));
        }
        let p = write_temp("large", content.as_bytes());
        let out = read_tail_window(&p, 1);
        // Every returned line must be a complete "line-NNNN" record, not a ragged fragment of
        // one — i.e. the leading partial line was dropped.
        assert!(!out.is_empty());
        for line in out.lines() {
            assert!(
                line.starts_with("line-") && line.len() == "line-0000".len(),
                "expected a complete line, got a fragment: {line:?}"
            );
        }
        // The tail is genuinely the END of the file, not the start.
        assert!(out.trim_end().ends_with("line-1999"));
        assert!(!out.contains("line-0000"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_tail_window_never_splits_a_multibyte_character() {
        // Build a file whose bytes force the seek boundary to land inside a multi-byte UTF-8
        // character: pad with ASCII newlines up to just under the 4 KiB clamp floor, then place
        // multi-byte (café) and 3-byte (CJK) characters straddling likely window-start offsets,
        // repeated so at least one lands exactly on the computed seek boundary.
        let mut content = String::new();
        for i in 0..500 {
            // "café 世界" mixes 2-byte and 3-byte UTF-8 sequences with ASCII on every line.
            content.push_str(&format!("line-{i:04} café 世界 end\n"));
        }
        let p = write_temp("utf8", content.as_bytes());
        for lines in [1usize, 5, 20, 100] {
            let out = read_tail_window(&p, lines);
            // Must be valid UTF-8 by construction (String), and must never contain the lossy
            // replacement character — proving the seek boundary never fractured a multi-byte
            // character into the output.
            assert!(
                !out.contains('\u{FFFD}'),
                "replacement character leaked into tail window output (lines={lines}): {out:?}"
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_tail_window_missing_file_fails_open_to_empty() {
        let p = std::env::temp_dir().join("indexa-web-test-tail-does-not-exist.log");
        let _ = std::fs::remove_file(&p);
        assert_eq!(read_tail_window(&p, 50), "");
    }
}
