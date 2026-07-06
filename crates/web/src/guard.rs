//! Request-guard middleware: defends the localhost web server against drive-by CSRF and
//! DNS-rebinding, and gates LAN (non-loopback) binds behind a shared bearer token.
//!
//! The server is unauthenticated by design on loopback (a local user already has the files), but
//! two browser-driven attacks reach it anyway:
//!   - **CSRF** — the state-changing POSTs take `Query` params (no JSON body) so they're
//!     CORS-"simple": a page the user visits can `fetch('http://127.0.0.1:7620/api/jobs/index?path=/')`
//!     and the mutation runs even though CORS blocks *reading* the response.
//!   - **DNS-rebinding** — an attacker domain re-pointed at 127.0.0.1 becomes same-origin with the
//!     server, so CORS no longer applies and the whole private index (`/api/export`, `/api/ask`,
//!     `/api/file`) is readable.
//!
//! This middleware closes both:
//!   - **Loopback bind (default):** reject any request whose `Host` header is present but NOT a
//!     loopback literal (a rebind arrives with the attacker's domain in `Host`), and reject
//!     state-changing methods whose `Origin`/`Referer` is present but cross-site. Absent
//!     `Host`/`Origin` (non-browser local tools like curl) is allowed — every browser drive-by /
//!     rebind vector carries both headers, so "validate if present" is complete for that threat
//!     model while leaving local scripts and the existing test suite working.
//!   - **Non-loopback bind (LAN):** require a shared bearer token on the private `/api/*` routes
//!     (the static shell carries no private data and loads freely). The token — generated and
//!     printed at startup, or `INDEXA_WEB_TOKEN` — defeats both vectors since an attacker never
//!     learns it. Accepted via `Authorization: Bearer <token>` or a `?token=` query param (so the
//!     initial navigation and `EventSource` streams, which can't set headers, still work).

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Per-server guard configuration, captured by the middleware layer.
#[derive(Clone)]
pub(crate) struct GuardConfig {
    /// `true` when the server is bound to a loopback address (Host/Origin checks); `false` for a
    /// LAN bind (bearer-token check on `/api/*`).
    loopback: bool,
    /// Shared bearer token required in LAN mode. `None` on loopback.
    token: Option<String>,
}

impl GuardConfig {
    /// Loopback bind (default): Host/Origin validation, no token.
    pub(crate) fn loopback() -> Self {
        Self {
            loopback: true,
            token: None,
        }
    }

    /// LAN (non-loopback) bind: require the shared bearer `token` on `/api/*`.
    pub(crate) fn lan(token: String) -> Self {
        Self {
            loopback: false,
            token: Some(token),
        }
    }
}

/// A random per-process bearer token: 128 bits from two v4 UUIDs (already a dep). Printed at
/// startup for LAN mode — not stored, not a user secret.
pub(crate) fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The middleware entry point (`axum::middleware::from_fn_with_state`).
pub(crate) async fn request_guard(
    State(cfg): State<GuardConfig>,
    req: Request,
    next: Next,
) -> Response {
    if cfg.loopback {
        if !host_ok(req.headers()) {
            return deny(
                StatusCode::FORBIDDEN,
                "request Host is not a loopback address (possible DNS-rebinding)",
            );
        }
        if is_state_changing(req.method()) && !origin_ok(req.headers()) {
            return deny(
                StatusCode::FORBIDDEN,
                "cross-origin state-changing request rejected (CSRF)",
            );
        }
    } else if req.uri().path().starts_with("/api/") && !token_ok(&cfg, &req) {
        return deny(
            StatusCode::UNAUTHORIZED,
            "missing or invalid token — pass ?token=<INDEXA_WEB_TOKEN> or Authorization: Bearer <token>",
        );
    }
    next.run(req).await
}

fn deny(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_owned()).into_response()
}

fn is_state_changing(m: &Method) -> bool {
    matches!(
        *m,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// A Host/authority string is loopback if it's `localhost` or parses to a loopback IP. Handles
/// `host:port`, `127.0.0.1`, `[::1]:port`, and bare `::1` via `SocketAddr`/`IpAddr` parsing.
fn authority_is_loopback(authority: &str) -> bool {
    if let Ok(sa) = authority.parse::<std::net::SocketAddr>() {
        return sa.ip().is_loopback();
    }
    if let Ok(ip) = authority.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    // Hostname, optionally `:port` — the only non-IP host we accept as loopback is `localhost`.
    let name = authority.rsplit_once(':').map_or(authority, |(a, _)| a);
    name.eq_ignore_ascii_case("localhost")
}

/// Host header must be loopback when present. Absent → allow (non-browser client; browsers always
/// send Host, so a rebind/drive-by always carries it).
fn host_ok(headers: &HeaderMap) -> bool {
    match headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        Some(h) => authority_is_loopback(h),
        None => true,
    }
}

/// Origin (or, failing that, Referer) must be same-origin loopback when present. Absent → allow
/// (same-origin navigations and non-browser tools omit it; a cross-site fetch/form always sends it).
fn origin_ok(headers: &HeaderMap) -> bool {
    let val = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok());
    match val {
        Some(o) => origin_is_loopback(o),
        None => true,
    }
}

fn origin_is_loopback(origin: &str) -> bool {
    // Strip `scheme://`, keep the authority up to the first `/`. "null"/opaque origins fail here.
    let after_scheme = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    authority_is_loopback(authority)
}

/// LAN token check: `Authorization: Bearer <token>` OR a `?token=` query param equals the
/// configured token. (Plain equality — timing attacks over the LAN on a random 128-bit token are
/// not the threat model.)
fn token_ok(cfg: &GuardConfig, req: &Request) -> bool {
    let Some(expected) = cfg.token.as_deref() else {
        return false;
    };
    if let Some(bearer) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|a| {
            a.strip_prefix("Bearer ")
                .or_else(|| a.strip_prefix("bearer "))
        })
    {
        if bearer == expected {
            return true;
        }
    }
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                if v == expected {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt; // Router::oneshot

    // Minimal router exercising the guard in isolation (no AppState): a private `/api/*` route
    // (GET + POST) and an open shell `/`.
    fn app(cfg: GuardConfig) -> Router {
        Router::new()
            .route("/api/x", get(|| async { "ok" }).post(|| async { "ok" }))
            .route("/", get(|| async { "shell" }))
            .layer(axum::middleware::from_fn_with_state(cfg, request_guard))
    }

    async fn status(cfg: GuardConfig, req: Request<Body>) -> StatusCode {
        app(cfg).oneshot(req).await.unwrap().status()
    }

    #[test]
    fn authority_loopback_matrix() {
        for ok in [
            "localhost",
            "localhost:7620",
            "127.0.0.1",
            "127.0.0.1:7620",
            "127.5.6.7:9",
            "[::1]:7620",
            "::1",
        ] {
            assert!(authority_is_loopback(ok), "should be loopback: {ok}");
        }
        for bad in [
            "evil.com",
            "evil.com:7620",
            "192.168.1.5:7620",
            "10.0.0.1",
            "0.0.0.0:7620",
        ] {
            assert!(!authority_is_loopback(bad), "should NOT be loopback: {bad}");
        }
    }

    #[tokio::test]
    async fn loopback_rejects_nonloopback_host_but_allows_loopback_and_absent() {
        // DNS-rebinding arrives with the attacker's domain in Host → 403.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::get("/api/x")
                    .header("host", "evil.com")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::FORBIDDEN
        );
        // Legit same-origin UI request (Host localhost) → passes.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::get("/api/x")
                    .header("host", "localhost:7620")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK
        );
        // Non-browser client with no Host (browsers always send it) → allowed.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::get("/api/x").body(Body::empty()).unwrap()
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn loopback_rejects_cross_origin_mutations_only() {
        // Cross-Origin POST (CSRF) → 403.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::post("/api/x")
                    .header("host", "127.0.0.1:7620")
                    .header("origin", "http://evil.com")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::FORBIDDEN
        );
        // Same-origin POST → OK.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::post("/api/x")
                    .header("host", "127.0.0.1:7620")
                    .header("origin", "http://localhost:7620")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK
        );
        // Cross-Origin GET is safe (not state-changing) → OK.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::get("/api/x")
                    .header("host", "127.0.0.1:7620")
                    .header("origin", "http://evil.com")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn lan_requires_token_on_api_only() {
        let token = "s3cr3t-token".to_owned();
        // No token → 401.
        assert_eq!(
            status(
                GuardConfig::lan(token.clone()),
                Request::get("/api/x").body(Body::empty()).unwrap()
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        // Correct bearer → OK.
        assert_eq!(
            status(
                GuardConfig::lan(token.clone()),
                Request::get("/api/x")
                    .header("authorization", "Bearer s3cr3t-token")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK
        );
        // Correct ?token= query (EventSource path) → OK.
        assert_eq!(
            status(
                GuardConfig::lan(token.clone()),
                Request::get("/api/x?token=s3cr3t-token")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK
        );
        // Wrong token → 401.
        assert_eq!(
            status(
                GuardConfig::lan(token.clone()),
                Request::get("/api/x")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        // The static shell needs no token (carries no private data).
        assert_eq!(
            status(
                GuardConfig::lan(token),
                Request::get("/").body(Body::empty()).unwrap()
            )
            .await,
            StatusCode::OK
        );
    }
}
