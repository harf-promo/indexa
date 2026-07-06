use axum::{
    http::header,
    response::{IntoResponse, Response},
};

use crate::{FAVICON_SVG, GEIST_MONO_WOFF2, GEIST_WOFF2, UI_CSS, UI_HTML, UI_JS};

pub(crate) async fn serve_ui() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_HTML,
    )
        .into_response()
}

pub(crate) async fn serve_ui_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        UI_CSS,
    )
        .into_response()
}

pub(crate) async fn serve_ui_js() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        UI_JS,
    )
        .into_response()
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
