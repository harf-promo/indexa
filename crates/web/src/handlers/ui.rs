use axum::{
    http::header,
    response::{IntoResponse, Response},
};

use crate::{FAVICON_SVG, UI_CSS, UI_HTML, UI_JS};

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
