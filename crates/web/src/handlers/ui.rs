use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::LazyLock;

use crate::{FAVICON_SVG, GEIST_MONO_WOFF2, GEIST_WOFF2, UI_CSS, UI_HTML, UI_JS};

/// A strong `ETag` = a hex FNV-1a hash of the embedded asset bytes, quoted. The bundle is embedded
/// at compile time, so each hash is constant per build — a repeat load whose `If-None-Match` matches
/// gets a `304 Not Modified` (no re-download of the bundle) while `Cache-Control: no-cache` still
/// forces a revalidation on every navigation, so a new build is always picked up.
fn etag_for(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

static HTML_ETAG: LazyLock<String> = LazyLock::new(|| etag_for(UI_HTML.as_bytes()));
static CSS_ETAG: LazyLock<String> = LazyLock::new(|| etag_for(UI_CSS.as_bytes()));
static JS_ETAG: LazyLock<String> = LazyLock::new(|| etag_for(UI_JS.as_bytes()));

/// Serve an embedded text asset with a content-hash `ETag` + `Cache-Control: no-cache`, returning
/// `304 Not Modified` when the client's `If-None-Match` already matches.
fn cached_asset(
    headers: &HeaderMap,
    etag: &str,
    content_type: &'static str,
    body: &'static str,
) -> Response {
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    if matches {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag.to_owned()),
                (header::CACHE_CONTROL, "no-cache".to_owned()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, content_type.to_owned()),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
            (header::ETAG, etag.to_owned()),
        ],
        body,
    )
        .into_response()
}

pub(crate) async fn serve_ui(headers: HeaderMap) -> Response {
    cached_asset(&headers, &HTML_ETAG, "text/html; charset=utf-8", UI_HTML)
}

pub(crate) async fn serve_ui_css(headers: HeaderMap) -> Response {
    cached_asset(&headers, &CSS_ETAG, "text/css; charset=utf-8", UI_CSS)
}

pub(crate) async fn serve_ui_js(headers: HeaderMap) -> Response {
    cached_asset(
        &headers,
        &JS_ETAG,
        "application/javascript; charset=utf-8",
        UI_JS,
    )
}

pub(crate) async fn serve_favicon() -> Response {
    (
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        FAVICON_SVG,
    )
        .into_response()
}

/// Long-cache header for the immutable embedded fonts (they only change on a redeploy).
const FONT_CACHE: &str = "public, max-age=31536000, immutable";

pub(crate) async fn serve_font_geist() -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, FONT_CACHE),
        ],
        GEIST_WOFF2,
    )
        .into_response()
}

pub(crate) async fn serve_font_geist_mono() -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, FONT_CACHE),
        ],
        GEIST_MONO_WOFF2,
    )
        .into_response()
}
