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
//!     loopback literal (a rebind arrives with the attacker's domain in `Host`), reject
//!     `/api/*` requests the browser marks `Sec-Fetch-Site: cross-site` (any method, so a
//!     drive-by GET cannot make the server do work), and reject state-changing methods whose
//!     `Origin`/`Referer` is present but cross-site. Absent
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
    allowed_origins: Vec<String>,
}

impl GuardConfig {
    /// Loopback bind (default): Host/Origin validation, no token.
    #[cfg(test)]
    pub(crate) fn loopback() -> Self {
        Self {
            loopback: true,
            token: None,
            allowed_origins: vec!["http://localhost:7620".to_owned()],
        }
    }

    pub(crate) fn loopback_origin(origin: String) -> Self {
        let port = origin
            .rsplit_once(':')
            .map(|(_, port)| port.to_owned())
            .unwrap_or_default();
        Self {
            loopback: true,
            token: None,
            allowed_origins: vec![origin, format!("http://localhost:{port}")],
        }
    }

    /// LAN (non-loopback) bind: require the shared bearer `token` on `/api/*`.
    pub(crate) fn lan(token: String) -> Self {
        Self {
            loopback: false,
            token: Some(token),
            allowed_origins: std::env::var("INDEXA_WEB_ORIGIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .into_iter()
                .collect(),
        }
    }

    pub(crate) fn lan_origin(token: String, port: u16, bound_origin: String) -> Self {
        let mut cfg = Self::lan(token);
        cfg.allowed_origins.push(bound_origin);
        cfg.allowed_origins.push(format!("http://localhost:{port}"));
        cfg
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
        if req.uri().path().starts_with("/api/") && is_cross_site_fetch(req.headers()) {
            return deny(
                StatusCode::FORBIDDEN,
                "cross-site request to the API rejected (Sec-Fetch-Site: cross-site)",
            );
        }
        if is_state_changing(req.method()) && !origin_ok(req.headers(), &cfg.allowed_origins) {
            return deny(
                StatusCode::FORBIDDEN,
                "cross-origin state-changing request rejected (CSRF)",
            );
        }
    } else {
        if is_state_changing(req.method()) && !origin_ok(req.headers(), &cfg.allowed_origins) {
            return deny(
                StatusCode::FORBIDDEN,
                "cross-origin state-changing request rejected (CSRF)",
            );
        }
        if req.uri().path().starts_with("/api/") && !token_ok(&cfg, &req) {
            return deny(
                StatusCode::UNAUTHORIZED,
                "missing or invalid token — pass ?token=<INDEXA_WEB_TOKEN> or Authorization: Bearer <token>",
            );
        }
    }
    next.run(req).await
}

fn deny(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_owned()).into_response()
}

/// `Sec-Fetch-Site: cross-site` marks a request a page on another site caused (an `<img>`,
/// `<script>`, `fetch`, form or navigation to the local server). CORS only stops such a page from
/// *reading* the reply; the handler still runs, so a drive-by GET could make the server hit
/// GitHub, render the whole export or call Ollama. Browsers set the header on every request and a
/// page cannot forge or strip it. `same-origin`, `same-site` and `none` (typed URL, bookmark) are
/// fine, and an absent header is allowed for curl, scripts and older browsers, matching the
/// "validate if present" rule of the Host and Origin checks above.
fn is_cross_site_fetch(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("cross-site"))
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
fn origin_ok(headers: &HeaderMap, allowed_origins: &[String]) -> bool {
    let val = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok());
    match val {
        Some(o) => allowed_origins
            .iter()
            .any(|expected| exact_same_origin(o, expected)),
        None => true,
    }
}

fn exact_same_origin(candidate: &str, expected: &str) -> bool {
    fn parts(value: &str) -> Option<(&str, &str)> {
        let (scheme, rest) = value.split_once("://")?;
        if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
            return None;
        }
        let authority = rest.split(['/', '?', '#']).next()?;
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        Some((scheme, authority))
    }
    match (parts(candidate), parts(expected)) {
        (
            Some((candidate_scheme, candidate_authority)),
            Some((expected_scheme, expected_authority)),
        ) => {
            candidate_scheme.eq_ignore_ascii_case(expected_scheme)
                && candidate_authority.eq_ignore_ascii_case(expected_authority)
        }
        _ => false,
    }
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
    async fn loopback_rejects_cross_origin_mutations_and_origin_less_gets_pass() {
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
        // A GET whose browser marks it cross-site is refused too (see
        // `loopback_rejects_cross_site_api_requests_by_fetch_metadata`); a GET with a foreign
        // Origin but NO fetch metadata (an old browser, a script) is still allowed, since only
        // the header can tell a drive-by from a local tool.
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

    #[test]
    fn origin_requires_exact_port_and_supports_ipv6() {
        let allowed = ["http://127.0.0.1:7620".to_owned()];
        assert!(exact_same_origin("http://127.0.0.1:7620", &allowed[0]));
        assert!(!exact_same_origin("http://127.0.0.1:7621", &allowed[0]));
        assert!(exact_same_origin("http://[::1]:7620", "http://[::1]:7620"));
        assert!(!exact_same_origin("http://[::1]:7621", "http://[::1]:7620"));
    }

    #[tokio::test]
    async fn loopback_mutation_rejects_other_port_and_accepts_bound_port() {
        let cfg = GuardConfig::loopback_origin("http://127.0.0.1:7620".into());
        let request = |origin: &str| {
            Request::post("/api/x")
                .header("host", "127.0.0.1:7620")
                .header("origin", origin)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            status(cfg.clone(), request("http://127.0.0.1:7621")).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(cfg, request("http://127.0.0.1:7620")).await,
            StatusCode::OK
        );
    }

    fn api_get(site: Option<&str>) -> Request<Body> {
        let mut b = Request::get("/api/x").header("host", "127.0.0.1:7620");
        if let Some(v) = site {
            b = b.header("sec-fetch-site", v);
        }
        b.body(Body::empty()).unwrap()
    }

    /// L1: browsers label every request with `Sec-Fetch-Site`. A page on another site that makes
    /// the local server do work (`<img src="http://127.0.0.1:7620/api/export">`) arrives as
    /// `cross-site` and is refused even though it is a GET; everything else keeps working.
    #[tokio::test]
    async fn loopback_rejects_cross_site_api_requests_by_fetch_metadata() {
        // Drive-by GET from another site -> 403.
        assert_eq!(
            status(GuardConfig::loopback(), api_get(Some("cross-site"))).await,
            StatusCode::FORBIDDEN
        );
        // Header values are case-insensitive tokens; stay robust to a differently cased or padded one.
        assert_eq!(
            status(GuardConfig::loopback(), api_get(Some(" Cross-Site "))).await,
            StatusCode::FORBIDDEN
        );
        // Cross-site POST is refused (also by the Origin check; this one has no Origin).
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::post("/api/x")
                    .header("host", "127.0.0.1:7620")
                    .header("sec-fetch-site", "cross-site")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::FORBIDDEN
        );
        // The UI itself (same-origin), a sibling on the same site, a typed URL / bookmark
        // (`none`), and a CLI client with no header at all all keep working.
        for ok in [Some("same-origin"), Some("same-site"), Some("none"), None] {
            assert_eq!(
                status(GuardConfig::loopback(), api_get(ok)).await,
                StatusCode::OK,
                "Sec-Fetch-Site {ok:?} must be allowed"
            );
        }
        // Only `/api/*` is guarded: a cross-site page may still load the static shell.
        assert_eq!(
            status(
                GuardConfig::loopback(),
                Request::get("/")
                    .header("host", "127.0.0.1:7620")
                    .header("sec-fetch-site", "cross-site")
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
