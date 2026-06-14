use axum::{
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use std::convert::Infallible;
use tokio_stream::{wrappers::WatchStream, StreamExt};

use crate::dto::{err_json, UpdateControlRequest, UpdateRequest};
use crate::update_control::{send_command, UpdateCommand};

/// `GET /api/update/check` — returns the current and latest version without
/// modifying anything. Network errors are swallowed so a transient GitHub
/// outage never breaks the page load.
pub(crate) async fn api_update_check() -> Response {
    // The desktop app updates itself (Tauri native updater); the web UI must NOT
    // offer a self-replace button there. Surface this so 15-update.js can render
    // the menu-bar pointer instead of "Update now".
    let desktop = std::env::var("INDEXA_DESKTOP").as_deref() == Ok("1");
    match indexa_update::check().await {
        Ok(info) => Json(serde_json::json!({
            "current":          info.current,
            "latest":           info.latest,
            "update_available": info.update_available,
            "desktop":          desktop,
        }))
        .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            // Return current version only; do not surface errors to the UI.
            Json(serde_json::json!({
                "current":          env!("CARGO_PKG_VERSION"),
                "latest":           null,
                "update_available": false,
                "desktop":          desktop,
                "error":            format!("{e:#}"),
            }))
            .into_response()
        }
    }
}

/// `POST /api/update/apply` — download the requested (or latest) release and
/// atomically replace the running binary.
///
/// Gated behind the `INDEXA_WEB_ALLOW_UPDATE=1` environment variable (mirrors
/// the `INDEXA_WEB_ALLOW_KEY_EDIT` gate on the keys endpoint). The `indexa
/// update` CLI command is always available as the ungated path.
///
/// After a successful update the new binary is on disk; the running server
/// keeps its old code in memory until the process is restarted.
pub(crate) async fn api_update_apply(Json(body): Json<UpdateRequest>) -> Response {
    // Hard refusal inside the desktop app: the binary self-replace corrupts the
    // `.app` bundle (downloads the headless CLI binary over the GUI Mach-O, strips
    // notarization). The desktop updates via its built-in updater. This holds even
    // if INDEXA_WEB_ALLOW_UPDATE is somehow set, and `indexa_update::apply` refuses
    // again as a third layer.
    if std::env::var("INDEXA_DESKTOP").as_deref() == Ok("1") {
        return err_json(
            StatusCode::FORBIDDEN,
            "The desktop app updates itself — use the menu-bar \"Check for Updates…\". \
             One-click web updates are disabled here to protect the app bundle.",
        );
    }
    if std::env::var("INDEXA_WEB_ALLOW_UPDATE").as_deref() != Ok("1") {
        return err_json(
            StatusCode::FORBIDDEN,
            "Set INDEXA_WEB_ALLOW_UPDATE=1 to enable one-click updates via the web UI, \
             or run `indexa update` in a terminal.",
        );
    }

    // Resolve tag: use the pin from the request body, or fetch the latest.
    let tag = match body.pin.as_deref() {
        Some(t) => t.to_string(),
        None => match indexa_update::check().await {
            Ok(info) => info.latest_tag,
            Err(e) => {
                return err_json(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("could not resolve latest release: {e:#}"),
                )
            }
        },
    };

    match indexa_update::apply(&tag).await {
        Ok(version) => {
            // The desktop app sets INDEXA_DESKTOP=1 and owns the relaunch.
            // Plain `indexa serve` requires a manual restart.
            let relaunch = if std::env::var("INDEXA_DESKTOP").as_deref() == Ok("1") {
                "desktop"
            } else {
                "manual"
            };
            Json(serde_json::json!({
                "updated":          true,
                "version":          version,
                "restart_required": true,
                "relaunch":         relaunch,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `POST /api/update/control` — the desktop in-app changelog modal's button choice (`start` |
/// `dismiss`). Wakes the desktop's `install_update` task waiting on `update_control::wait_for_command`.
/// Gated to the desktop app (`INDEXA_DESKTOP=1`): under plain `indexa serve` there's no updater task
/// listening, so it returns 403 rather than silently no-op.
pub(crate) async fn api_update_control(Json(body): Json<UpdateControlRequest>) -> Response {
    if std::env::var("INDEXA_DESKTOP").as_deref() != Ok("1") {
        return err_json(
            StatusCode::FORBIDDEN,
            "update control is only available inside the desktop app",
        );
    }
    let cmd = match body.action.as_str() {
        "start" => UpdateCommand::Start,
        "dismiss" => UpdateCommand::Dismiss,
        other => {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("unknown update action '{other}' (expected 'start' or 'dismiss')"),
            )
        }
    };
    send_command(cmd);
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `GET /api/update/progress/stream` — SSE that emits the current update-progress snapshot
/// immediately, then on every change. Written only by the desktop app (Tauri updater / CLI
/// downloader); under plain `indexa serve` the value stays `idle` and the web overlay never shows.
/// Mirrors `api_telemetry_stream` — `WatchStream` yields the latest value to each new subscriber,
/// so a reconnecting client has no startup gap and needs no replay/dedup machinery.
pub(crate) async fn api_update_progress_stream() -> impl IntoResponse {
    let stream = WatchStream::new(crate::update_progress::subscribe()).map(|p| {
        let data = serde_json::to_string(&p).unwrap_or_default();
        Ok::<Event, Infallible>(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::new())
}
