//! Shared HTTP plumbing for Indexa's network adapters (`indexa-llm`, `indexa-embed`).
//!
//! One source of truth for client construction and transient-failure retry policy —
//! these were duplicated per adapter crate and had already started to require
//! synchronized edits. Kept out of `indexa-core` on purpose: core is network-free.

/// Build a reqwest client with a finite request + connect timeout, shared by every
/// network adapter. Without a timeout a stalled endpoint (no FIN, no bytes) hangs the
/// call indefinitely — and these run inside the indexing worker and web/MCP request
/// paths. `expect` is appropriate: `build()` only fails on unrecoverable rustls TLS
/// init, and never silently yields a no-timeout client (unlike `.unwrap_or_default()`).
pub fn http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("building reqwest client (rustls TLS init)")
}

/// An IP an outbound "fetch a user-supplied URL" request must never reach — the SSRF blocklist:
/// loopback, private (RFC1918), link-local (incl. the cloud-metadata `169.254.169.254`), CGNAT,
/// unspecified, broadcast, and multicast — plus the IPv6 equivalents. Keeps a benign-looking URL
/// from being used to reach internal services or the cloud metadata endpoint.
fn ip_is_blocked(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || v6.to_ipv4_mapped().is_some_and(|m| ip_is_blocked(IpAddr::V4(m)))
        }
    }
}

/// Validate a URL for an outbound fetch of a **user-supplied** target: `http`/`https` only, and its
/// host must resolve to at least one address, none in a blocked range ([`ip_is_blocked`]). Returns a
/// human-readable reason on refusal. This is the SSRF guard for `pack add-url`; the shared
/// [`http_client`] deliberately does NOT apply it (provider endpoints come from trusted config).
pub fn validate_public_url(url: &reqwest::Url) -> Result<(), String> {
    use std::net::{IpAddr, ToSocketAddrs};
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("refusing non-http(s) URL scheme '{other}'")),
    }
    let host = url.host_str().ok_or_else(|| "URL has no host".to_owned())?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("resolving {host}: {e}"))?
            .map(|sa| sa.ip())
            .collect()
    };
    if addrs.is_empty() {
        return Err(format!("{host} did not resolve"));
    }
    if let Some(ip) = addrs.iter().find(|ip| ip_is_blocked(**ip)) {
        return Err(format!(
            "{host} resolves to a non-public address ({ip}); blocked to prevent SSRF into \
             loopback/link-local/private ranges"
        ));
    }
    Ok(())
}

/// Parse + [`validate_public_url`] a URL string (used on the initial fetch target).
pub fn validate_public_url_str(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL '{url}': {e}"))?;
    validate_public_url(&parsed)
}

/// A reqwest client for fetching **user-supplied** URLs (`pack add-url`): finite timeouts plus a
/// redirect policy that [`validate_public_url`]s EVERY hop (capped at 5), so a benign first host
/// can't `3xx` to `169.254.169.254` / `localhost` / an internal service. Kept separate from
/// [`http_client`] — mutating the shared client's redirect policy would break the legitimate
/// redirects every LLM/embed provider relies on.
pub fn ssrf_guarded_client(timeout_secs: u64) -> reqwest::Client {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects (max 5)".to_owned());
        }
        match validate_public_url(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    });
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(policy)
        .build()
        .expect("building SSRF-guarded reqwest client (rustls TLS init)")
}

/// HTTP status codes worth retrying — transient server errors and rate limits.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Backoff before retry `attempt` (0-based): honor `Retry-After` if present (capped at 30s),
/// otherwise exponential `0.5s · 2^attempt`, capped at 8s.
pub fn backoff_delay(
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> std::time::Duration {
    use std::time::Duration;
    if let Some(ra) = retry_after {
        return ra.min(Duration::from_secs(30));
    }
    (Duration::from_millis(500) * 2u32.saturating_pow(attempt)).min(Duration::from_secs(8))
}

/// Send a freshly-built request with bounded retries on transient failures (retryable status
/// codes + connection/timeout errors). `build` is called once per attempt because `send()`
/// consumes the builder. Bulk indexing routinely hits 429/503 from cloud providers; without
/// this each such response permanently fails that item.
pub async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    max_retries: u32,
) -> reqwest::Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        match build().send().await {
            Ok(resp) if attempt < max_retries && is_retryable_status(resp.status().as_u16()) => {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);
                tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
                attempt += 1;
            }
            Ok(resp) => return Ok(resp),
            Err(e)
                if attempt < max_retries
                    && (e.is_timeout() || e.is_connect() || e.is_request()) =>
            {
                tokio::time::sleep(backoff_delay(attempt, None)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Read a response body into a `String`, refusing to buffer more than `max_bytes`. Streams the
/// body chunk-by-chunk (no reqwest `stream` feature needed) and errors the moment the accumulated
/// size would exceed the cap, so a hostile/runaway endpoint can never blow up memory by sending an
/// arbitrarily large body. `what` names the source for the error/context message.
pub async fn read_body_capped(
    mut resp: reqwest::Response,
    max_bytes: usize,
    what: &str,
) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind};
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| Error::other(format!("reading {what}: {e}")))?;
        let Some(chunk) = chunk else { break };
        if buf.len() + chunk.len() > max_bytes {
            return Err(Error::other(format!(
                "remote source {what} exceeded the {} MB limit — refusing to buffer it",
                max_bytes / (1024 * 1024)
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| {
        Error::new(
            ErrorKind::InvalidData,
            format!("{what} was not valid UTF-8"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_guard_blocks_private_loopback_and_nonhttp() {
        // IP-literal targets need no DNS — deterministic.
        for blocked in [
            "http://127.0.0.1/x",
            "http://169.254.169.254/latest/meta-data/", // AWS/GCP metadata (link-local)
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://100.64.0.1/", // CGNAT
            "http://[::1]/",
            "http://0.0.0.0/",
            "ftp://example.com/", // non-http scheme
            "file:///etc/passwd", // non-http scheme
        ] {
            assert!(
                validate_public_url_str(blocked).is_err(),
                "should be blocked: {blocked}"
            );
        }
        // Public IP literals are allowed (no resolution needed).
        for ok in ["http://8.8.8.8/", "https://93.184.216.34/"] {
            assert!(
                validate_public_url_str(ok).is_ok(),
                "should be allowed: {ok}"
            );
        }
    }

    #[test]
    fn retryable_statuses() {
        for s in [408, 425, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(s), "{s} must be retryable");
        }
        for s in [200, 201, 301, 400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(s), "{s} must not be retryable");
        }
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        use std::time::Duration;
        assert_eq!(backoff_delay(0, None), Duration::from_millis(500));
        assert_eq!(backoff_delay(1, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(2));
        assert_eq!(backoff_delay(10, None), Duration::from_secs(8), "capped");
        assert_eq!(
            backoff_delay(0, Some(Duration::from_secs(120))),
            Duration::from_secs(30),
            "Retry-After capped at 30s"
        );
        assert_eq!(
            backoff_delay(5, Some(Duration::from_secs(3))),
            Duration::from_secs(3),
            "Retry-After honored over exponential"
        );
    }
}
