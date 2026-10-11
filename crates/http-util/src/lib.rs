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

/// Like [`http_client`], but with a `User-Agent` header — for provider-neutral network
/// adapters that talk to a public API/CDN identifying themselves as Indexa (GitHub
/// releases, the remote plugin directory), as opposed to [`http_client`]'s LLM/embed
/// providers, which don't send one.
///
/// Unlike `http_client`, this returns a `Result` rather than panicking on a build
/// failure: its current caller (the remote plugin directory fetch) is an explicitly
/// best-effort, falls-back-to-a-local-copy operation — a rustls TLS-init failure in a
/// minimal/headless environment must be a recoverable `Err`, not a process-crashing
/// panic, so the caller's existing fallback-to-embedded-list path actually triggers.
pub fn http_client_with_user_agent(
    timeout_secs: u64,
    user_agent: &str,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
}

/// IPv4 ranges an outbound "fetch a user-supplied URL" request must never reach (`(network,
/// prefix length)`): everything that is not ordinary public unicast.
const BLOCKED_V4: &[(std::net::Ipv4Addr, u32)] = {
    use std::net::Ipv4Addr as A;
    &[
        (A::new(0, 0, 0, 0), 8),       // "this network", incl. unspecified 0.0.0.0
        (A::new(10, 0, 0, 0), 8),      // private (RFC 1918)
        (A::new(100, 64, 0, 0), 10),   // CGNAT shared address space (RFC 6598)
        (A::new(127, 0, 0, 0), 8),     // loopback
        (A::new(169, 254, 0, 0), 16),  // link-local, incl. cloud metadata 169.254.169.254
        (A::new(172, 16, 0, 0), 12),   // private (RFC 1918)
        (A::new(192, 0, 0, 0), 24),    // IETF protocol assignments (RFC 6890)
        (A::new(192, 0, 2, 0), 24),    // TEST-NET-1 documentation
        (A::new(192, 31, 196, 0), 24), // AS112-v4 (RFC 7535)
        (A::new(192, 52, 193, 0), 24), // AMT (RFC 7450)
        (A::new(192, 88, 99, 0), 24),  // deprecated 6to4 relay anycast (RFC 7526)
        (A::new(192, 168, 0, 0), 16),  // private (RFC 1918)
        (A::new(192, 175, 48, 0), 24), // AS112 direct delegation (RFC 7534)
        (A::new(198, 18, 0, 0), 15),   // benchmarking (RFC 2544)
        (A::new(198, 51, 100, 0), 24), // TEST-NET-2 documentation
        (A::new(203, 0, 113, 0), 24),  // TEST-NET-3 documentation
        (A::new(224, 0, 0, 0), 4),     // multicast
        (A::new(240, 0, 0, 0), 4),     // reserved, incl. broadcast 255.255.255.255
    ]
};

/// IPv6 ranges blocked outright. Prefixes that embed an IPv4 address a client really reaches
/// (IPv4-mapped `::ffff:0:0/96`, NAT64 `64:ff9b::/96`, 6to4 `2002::/16`) are not listed here:
/// [`ip_is_blocked`] checks the embedded IPv4 address instead, so DNS64/NAT64 networks still reach
/// public sites while `64:ff9b::7f00:1` (= 127.0.0.1) stays blocked.
const BLOCKED_V6: &[(std::net::Ipv6Addr, u32)] = {
    use std::net::Ipv6Addr as A;
    &[
        (A::new(0, 0, 0, 0, 0, 0, 0, 0), 96), // IPv4-compatible (deprecated), incl. :: and ::1
        (A::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48), // local-use NAT64 (RFC 8215)
        (A::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64), // discard-only (RFC 6666)
        (A::new(0x100, 0, 0, 1, 0, 0, 0, 0), 64), // dummy prefix (RFC 9780)
        (A::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32), // Teredo
        (A::new(0x2001, 1, 0, 0, 0, 0, 0, 0), 32), // PCP/TURN/DNS-SD anycast (RFC 7723, 8155, 9665)
        (A::new(0x2001, 2, 0, 0, 0, 0, 0, 0), 48), // benchmarking (RFC 5180)
        (A::new(0x2001, 4, 0x112, 0, 0, 0, 0, 0), 48), // AS112-v6 (RFC 7535)
        (A::new(0x2001, 0x10, 0, 0, 0, 0, 0, 0), 28), // ORCHID (deprecated, RFC 4843)
        (A::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0), 28), // ORCHIDv2 (RFC 7343)
        (A::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32), // documentation
        (A::new(0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0), 48), // AS112 direct delegation (RFC 7534)
        (A::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20), // documentation (RFC 9637)
        (A::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16), // SRv6 SIDs (RFC 9602)
        (A::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7), // unique-local
        (A::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10), // link-local
        (A::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10), // site-local (deprecated)
        (A::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8), // multicast
    ]
};

/// An IP an outbound "fetch a user-supplied URL" request must never reach — the SSRF blocklist
/// ([`BLOCKED_V4`], [`BLOCKED_V6`], and any IPv4 address embedded in an IPv4-mapped, NAT64 or 6to4
/// IPv6 address). Keeps a benign-looking URL from being used to reach internal services or the
/// cloud metadata endpoint.
fn ip_is_blocked(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match ip {
        IpAddr::V4(v4) => BLOCKED_V4.iter().any(|&(net, len)| {
            (u32::from(v4) ^ u32::from(net))
                .checked_shr(32 - len)
                .unwrap_or(0)
                == 0
        }),
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let in_prefix = |net: Ipv6Addr, len: u32| {
                (bits ^ u128::from(net)).checked_shr(128 - len).unwrap_or(0) == 0
            };
            let s = v6.segments();
            let embedded = if in_prefix(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96)
                || in_prefix(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96)
            {
                // IPv4-mapped / NAT64: the IPv4 address is the low 32 bits.
                Some(Ipv4Addr::from(bits as u32))
            } else if s[0] == 0x2002 {
                // 6to4: the IPv4 address is bits 16..48.
                Some(Ipv4Addr::from(((s[1] as u32) << 16) | s[2] as u32))
            } else {
                None
            };
            BLOCKED_V6.iter().any(|&(net, len)| in_prefix(net, len))
                || embedded.is_some_and(|e| ip_is_blocked(IpAddr::V4(e)))
        }
    }
}

/// Why a user-supplied fetch target was refused. Returned by the guarded client's DNS resolver, so
/// it travels inside the `reqwest::Error` source chain; [`send_with_retry`] looks for it there and
/// never retries a refusal.
#[derive(Debug)]
struct SsrfBlocked(String);

impl std::fmt::Display for SsrfBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SsrfBlocked {}

/// The guarded client's DNS lookup said the user-supplied host does not exist
/// ([`is_no_such_host`]). Final like [`SsrfBlocked`]: a mistyped or nonexistent host fails fast
/// instead of being retried with backoff. Any other lookup failure stays a plain, retryable I/O
/// error.
#[derive(Debug)]
struct Unresolvable {
    host: String,
    source: std::io::Error,
}

impl std::fmt::Display for Unresolvable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not resolve {}", self.host)
    }
}

impl std::error::Error for Unresolvable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Does `err`'s source chain contain a `T`?
fn chain_has<T: std::error::Error + 'static>(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<T>() {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Does `err`'s source chain contain an [`SsrfBlocked`] refusal?
fn is_ssrf_block(err: &(dyn std::error::Error + 'static)) -> bool {
    chain_has::<SsrfBlocked>(err)
}

/// Is `err` a refusal by [`ssrf_guarded_client`]'s guard (a blocked resolution or redirect hop),
/// as opposed to a network failure? Lets callers word their error for a deliberate block.
pub fn is_ssrf_refusal(err: &reqwest::Error) -> bool {
    is_ssrf_block(err)
}

/// Is a failed host lookup a definitive "no such host" answer rather than a transient failure?
///
/// `std`'s resolver gives no typed kind for this. On Unix a `getaddrinfo` failure carries only the
/// `gai_strerror` text, so this matches the `EAI_NONAME` / `EAI_NODATA` strings of glibc, musl and
/// macOS; on Windows the raw `WSAHOST_NOT_FOUND` / `WSANO_DATA` codes. Everything else, including
/// `EAI_AGAIN` ("Temporary failure in name resolution"), `EAI_FAIL`, timeouts, and a message
/// translated by a non-English glibc locale, is treated as transient, so the worst a miss costs is
/// a retry.
fn is_no_such_host(err: &std::io::Error) -> bool {
    const NOT_FOUND: [&str; 5] = [
        "name or service not known",                    // glibc EAI_NONAME
        "no address associated with hostname",          // glibc EAI_NODATA
        "name does not resolve",                        // musl EAI_NONAME
        "name has no usable address",                   // musl EAI_NODATA
        "nodename nor servname provided, or not known", // macOS/BSD EAI_NONAME
    ];
    // WSAHOST_NOT_FOUND, WSANO_DATA.
    if cfg!(windows) && matches!(err.raw_os_error(), Some(11001 | 11004)) {
        return true;
    }
    let msg = err.to_string().to_ascii_lowercase();
    NOT_FOUND.iter().any(|m| msg.contains(m))
}

/// Accept a host's resolved addresses only if there is at least one and NONE is blocked — a mixed
/// answer is refused outright rather than filtered, so an attacker can't rely on connection
/// fallback from a dead public address to a live private one.
fn vet_resolved(
    host: &str,
    addrs: Vec<std::net::IpAddr>,
) -> Result<Vec<std::net::IpAddr>, SsrfBlocked> {
    if addrs.is_empty() {
        return Err(SsrfBlocked(format!("{host} did not resolve")));
    }
    if let Some(ip) = addrs.iter().find(|ip| ip_is_blocked(**ip)) {
        return Err(SsrfBlocked(format!(
            "{host} resolves to a non-public address ({ip}); blocked to prevent SSRF into \
             loopback/link-local/private ranges"
        )));
    }
    Ok(addrs)
}

/// Validate a URL for an outbound fetch of a **user-supplied** target without touching DNS:
/// `http`/`https` only, a host must be present, and an IP-literal host (incl. forms like
/// `http://2130706433/`, which the URL parser normalises to `127.0.0.1`) must not be in a blocked
/// range ([`ip_is_blocked`]). Returns a human-readable reason on refusal.
///
/// Hostnames are deliberately NOT resolved here: resolving now and letting the client resolve again
/// at connect time is a DNS-rebinding window. [`ssrf_guarded_client`]'s resolver vets the addresses
/// it actually connects to. The shared [`http_client`] applies none of this (provider endpoints come
/// from trusted config).
pub fn validate_public_url(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("refusing non-http(s) URL scheme '{other}'")),
    }
    let host = url.host_str().ok_or_else(|| "URL has no host".to_owned())?;
    // `host_str` brackets IPv6 literals; IPv4 literals are already normalised to dotted-quad.
    let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    else {
        return Ok(()); // a hostname — vetted at connect time by the guarded resolver
    };
    if ip_is_blocked(ip) {
        return Err(format!(
            "{ip} is a non-public address; blocked to prevent SSRF into loopback/link-local/private \
             ranges"
        ));
    }
    Ok(())
}

/// Parse + [`validate_public_url`] a URL string (used on the initial fetch target).
pub fn validate_public_url_str(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL '{url}': {e}"))?;
    validate_public_url(&parsed)
}

/// Host → addresses lookup behind [`SsrfGuardResolver`]: the system resolver in production, a stub
/// in tests.
type Lookup = std::sync::Arc<
    dyn Fn(
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::io::Result<Vec<std::net::IpAddr>>> + Send>,
        > + Send
        + Sync,
>;

fn system_lookup() -> Lookup {
    std::sync::Arc::new(|host: String| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|sa| sa.ip())
                .collect())
        })
    })
}

/// The guarded client's DNS resolver: resolves a host ONCE, [`vet_resolved`]s every address, and
/// hands reqwest exactly those vetted addresses to connect to — so there is no second lookup an
/// attacker's DNS could answer differently (rebinding). Every hostname connection goes through it,
/// including redirect hops; IP-literal hosts skip DNS and are vetted by the redirect policy.
struct SsrfGuardResolver {
    lookup: Lookup,
}

impl reqwest::dns::Resolve for SsrfGuardResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let lookup = (self.lookup)(host.clone());
        Box::pin(async move {
            // A lookup failure is not an SSRF refusal. A definitive "no such host" is final; any
            // other failure (a DNS blip, a resolver timeout) stays a plain, retryable I/O error.
            let addrs = lookup
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    if is_no_such_host(&e) {
                        Box::new(Unresolvable {
                            host: host.clone(),
                            source: e,
                        })
                    } else {
                        Box::new(std::io::Error::new(
                            e.kind(),
                            format!("resolving {host}: {e}"),
                        ))
                    }
                })?;
            let vetted = vet_resolved(&host, addrs)?;
            let addrs: reqwest::dns::Addrs = Box::new(
                vetted
                    .into_iter()
                    .map(|ip| std::net::SocketAddr::new(ip, 0)),
            );
            Ok(addrs)
        })
    }
}

/// A reqwest client for fetching **user-supplied** URLs (`pack add-url`): finite timeouts, a
/// DNS resolver that vets and pins every address it connects to ([`SsrfGuardResolver`]), and a
/// redirect policy that [`validate_public_url`]s EVERY hop (capped at 5), so a benign first host
/// can't `3xx` to `169.254.169.254` / `localhost` / an internal service. Proxies are disabled: a
/// proxy resolves the target itself, out of the guard's sight. Kept separate from [`http_client`] —
/// mutating the shared client's redirect policy would break the legitimate redirects every LLM/embed
/// provider relies on.
pub fn ssrf_guarded_client(timeout_secs: u64) -> reqwest::Client {
    ssrf_guarded_client_with_lookup(timeout_secs, system_lookup())
}

fn ssrf_guarded_client_with_lookup(timeout_secs: u64, lookup: Lookup) -> reqwest::Client {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects (max 5)".to_owned());
        }
        match validate_public_url(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(SsrfBlocked(reason)),
        }
    });
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(policy)
        .dns_resolver(SsrfGuardResolver { lookup })
        .no_proxy()
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
/// this each such response permanently fails that item. An SSRF refusal from
/// [`ssrf_guarded_client`] surfaces as a connect error but is final, so it is never retried; nor is
/// that client's definitive "no such host" lookup result. Its transient lookup failures are retried.
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
                    && (e.is_timeout() || e.is_connect() || e.is_request())
                    && !is_ssrf_block(&e)
                    && !chain_has::<Unresolvable>(&e) =>
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
            "http://2130706433/",           // 127.0.0.1 in decimal form
            "http://0x7f000001/",           // 127.0.0.1 as one hex number
            "http://0x7f.1/",               // 127.0.0.1, hex + shortened
            "http://0177.0.0.1/",           // 127.0.0.1, octal first octet
            "http://017700000001/",         // 127.0.0.1 as one octal number
            "http://127.1/",                // 127.0.0.1, shortened
            "http://2852039166/",           // 169.254.169.254 in decimal form
            "http://0xa9.0xfe.0xa9.0xfe/",  // 169.254.169.254, hex octets
            "http://0251.0376.0251.0376/",  // 169.254.169.254, octal octets
            "http://012.0.0.1/",            // 10.0.0.1, octal first octet
            "http://0xc0a80101/",           // 192.168.1.1 as one hex number
            "http://[::ffff:127.0.0.1]/",   // IPv4-mapped loopback
            "http://[64:ff9b::a9fe:a9fe]/", // NAT64-embedded metadata address
            "ftp://example.com/",           // non-http scheme
            "file:///etc/passwd",           // non-http scheme
        ] {
            assert!(
                validate_public_url_str(blocked).is_err(),
                "should be blocked: {blocked}"
            );
        }
        // Public IP literals are allowed; hostnames are left to the connect-time resolver.
        for ok in [
            "http://8.8.8.8/",
            "https://93.184.216.34/",
            "https://example.com/",
        ] {
            assert!(
                validate_public_url_str(ok).is_ok(),
                "should be allowed: {ok}"
            );
        }
    }

    #[test]
    fn ip_blocklist_covers_every_non_public_range() {
        use std::net::IpAddr;
        let blocked = [
            "0.1.2.3",                // 0.0.0.0/8
            "0.0.0.0",                // unspecified
            "10.1.2.3",               // RFC 1918
            "100.64.0.0",             // CGNAT 100.64.0.0/10, first
            "100.127.255.255",        // CGNAT, last
            "127.0.0.1",              // loopback
            "127.255.255.254",        // loopback /8
            "169.254.169.254",        // link-local / cloud metadata
            "172.16.0.1",             // RFC 1918
            "172.31.255.255",         // RFC 1918, last
            "192.0.0.8",              // IETF protocol assignments 192.0.0.0/24
            "192.0.2.1",              // TEST-NET-1
            "192.31.196.1",           // AS112-v4
            "192.52.193.1",           // AMT
            "192.88.99.1",            // 6to4 relay anycast
            "192.168.1.1",            // RFC 1918
            "192.175.48.6",           // AS112 direct delegation
            "198.18.0.0",             // benchmarking 198.18.0.0/15, first
            "198.19.255.255",         // benchmarking, last
            "198.51.100.7",           // TEST-NET-2
            "203.0.113.7",            // TEST-NET-3
            "224.0.0.1",              // multicast
            "239.255.255.250",        // multicast (SSDP)
            "240.0.0.1",              // reserved 240.0.0.0/4
            "255.255.255.255",        // broadcast
            "::",                     // unspecified
            "::1",                    // loopback
            "::7f00:1",               // IPv4-compatible (deprecated) ::/96
            "::808:808",              // IPv4-compatible, even embedding a public address
            "::ffff:127.0.0.1",       // IPv4-mapped loopback
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "::ffff:10.0.0.1",        // IPv4-mapped private
            "64:ff9b::7f00:1",        // NAT64 64:ff9b::/96 embedding 127.0.0.1
            "64:ff9b::a9fe:a9fe",     // NAT64 embedding 169.254.169.254
            "64:ff9b:1::1",           // local-use NAT64 64:ff9b:1::/48
            "100::1",                 // discard-only 100::/64
            "100:0:0:1::1",           // dummy prefix 100:0:0:1::/64
            "2001::1",                // Teredo 2001::/32
            "2001:1::1",              // PCP anycast 2001:1::/32
            "2001:2::1",              // benchmarking 2001:2::/48
            "2001:4:112::1",          // AS112-v6 2001:4:112::/48
            "2001:10::1",             // ORCHID 2001:10::/28, first
            "2001:1f:ffff::1",        // ORCHID, last
            "2001:20::1",             // ORCHIDv2 2001:20::/28
            "2001:2f:ffff::1",        // ORCHIDv2, last
            "2001:db8::1",            // documentation
            "2002:7f00:1::1",         // 6to4 2002::/16 embedding 127.0.0.1
            "2002:c0a8:101::",        // 6to4 embedding 192.168.1.1
            "2620:4f:8000::1",        // AS112 direct delegation 2620:4f:8000::/48
            "3fff::1",                // documentation 3fff::/20
            "5f00::1",                // SRv6 SIDs 5f00::/16
            "fc00::1",                // unique-local fc00::/7
            "fdff:ffff::1",           // unique-local, upper half
            "fe80::1",                // link-local fe80::/10
            "febf:ffff::1",           // link-local, last
            "fec0::1",                // site-local (deprecated)
            "ff02::1",                // multicast ff00::/8
            "ff0e::1",                // global-scope multicast
        ];
        for ip in blocked {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(ip_is_blocked(parsed), "should be blocked: {ip}");
        }
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",  // just below CGNAT
            "100.128.0.0",     // just above CGNAT
            "172.32.0.1",      // just above 172.16/12
            "192.0.1.1",       // between 192.0.0/24 and TEST-NET-1
            "198.17.255.255",  // just below 198.18/15
            "198.20.0.0",      // just above 198.18/15
            "223.255.255.255", // just below multicast
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",       // IPv4-mapped public
            "64:ff9b::808:808",     // NAT64 to a public address (DNS64 networks)
            "2002:808:808::1",      // 6to4 embedding a public address
            "2001:4860:4860::8888", // 2001:4860::/32 is outside Teredo 2001::/32
            "2001:3::1",            // between the 2001:2::/48 and ORCHID blocks
            "2001:30::1",           // just above ORCHIDv2
            "2620:4f:8001::1",      // just above the AS112 v6 delegation
            "192.31.197.1",         // just above AS112-v4
            "192.175.49.1",         // just above the AS112 v4 delegation
            "5f01::1",              // just above SRv6 SIDs
        ];
        for ip in public {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(!ip_is_blocked(parsed), "should be allowed: {ip}");
        }
    }

    /// A [`Lookup`] stub answering from `answers` in order (repeating the last one), counting calls.
    fn stub_lookup(
        answers: Vec<Vec<&'static str>>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Lookup {
        std::sync::Arc::new(move |_host: String| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answer: Vec<std::net::IpAddr> = answers[n.min(answers.len() - 1)]
                .iter()
                .map(|ip| ip.parse().unwrap())
                .collect();
            Box::pin(async move { Ok(answer) })
        })
    }

    async fn resolve_with(
        resolver: &SsrfGuardResolver,
        host: &str,
    ) -> Result<Vec<std::net::SocketAddr>, String> {
        use reqwest::dns::Resolve;
        resolver
            .resolve(host.parse().unwrap())
            .await
            .map(|addrs| addrs.collect())
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn resolver_pins_vetted_addresses_and_refuses_a_rebound_answer() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A rebinding DNS server: public on the first query, loopback on the next.
        let resolver = SsrfGuardResolver {
            lookup: stub_lookup(
                vec![vec!["93.184.216.34"], vec!["127.0.0.1"]],
                calls.clone(),
            ),
        };
        // First resolution: the client connects to exactly the address that was vetted.
        let first = resolve_with(&resolver, "rebind.test").await.unwrap();
        assert_eq!(first, vec!["93.184.216.34:0".parse().unwrap()]);
        // Any later resolution (a new connection, a redirect hop) is vetted again — the rebound
        // private answer is refused instead of being connected to.
        let second = resolve_with(&resolver, "rebind.test").await.unwrap_err();
        assert!(second.contains("127.0.0.1"), "{second}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn resolver_refuses_mixed_and_empty_answers() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolver = SsrfGuardResolver {
            lookup: stub_lookup(vec![vec!["93.184.216.34", "10.0.0.1"]], calls.clone()),
        };
        let mixed = resolve_with(&resolver, "mixed.test").await.unwrap_err();
        assert!(mixed.contains("10.0.0.1"), "{mixed}");
        let resolver = SsrfGuardResolver {
            lookup: stub_lookup(vec![vec![]], calls),
        };
        let empty = resolve_with(&resolver, "empty.test").await.unwrap_err();
        assert!(empty.contains("did not resolve"), "{empty}");
    }

    #[tokio::test]
    async fn guarded_client_never_connects_to_a_private_resolution_and_does_not_retry() {
        // A real local listener the request would land on if the guard let it through.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client =
            ssrf_guarded_client_with_lookup(5, stub_lookup(vec![vec!["127.0.0.1"]], calls.clone()));
        let url = format!("http://rebind.test:{port}/");
        // The initial no-DNS check passes a hostname through; the connect-time resolver refuses it.
        assert!(validate_public_url_str(&url).is_ok());
        let err = send_with_retry(|| client.get(&url), 3).await.unwrap_err();
        assert!(
            is_ssrf_block(&err),
            "expected an SSRF refusal, got: {err:?}"
        );
        assert!(is_ssrf_refusal(&err), "the public check must agree");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an SSRF refusal must not be retried"
        );
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the guarded client must never open a connection to the private address"
        );
    }

    /// A [`Lookup`] stub that always fails with `msg` (as `std` reports a `getaddrinfo` failure),
    /// counting calls.
    fn failing_lookup(
        msg: &'static str,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Lookup {
        std::sync::Arc::new(move |_host: String| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Err(std::io::Error::other(msg)) })
        })
    }

    #[tokio::test]
    async fn guarded_client_does_not_retry_an_unresolvable_host() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lookup = failing_lookup(
            "failed to lookup address information: Name or service not known",
            calls.clone(),
        );
        let client = ssrf_guarded_client_with_lookup(5, lookup);
        let err = send_with_retry(|| client.get("http://no-such-host.test/"), 3)
            .await
            .unwrap_err();
        assert!(chain_has::<Unresolvable>(&err), "got: {err:?}");
        assert!(
            !is_ssrf_block(&err),
            "a lookup failure is not an SSRF refusal"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an unresolvable host must not be retried"
        );
    }

    #[tokio::test]
    async fn guarded_client_retries_a_transient_lookup_failure() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lookup = failing_lookup(
            "failed to lookup address information: Temporary failure in name resolution",
            calls.clone(),
        );
        let client = ssrf_guarded_client_with_lookup(5, lookup);
        // One retry keeps the test to a single 0.5 s backoff.
        let err = send_with_retry(|| client.get("http://flaky-dns.test/"), 1)
            .await
            .unwrap_err();
        assert!(!chain_has::<Unresolvable>(&err), "got: {err:?}");
        assert!(!is_ssrf_refusal(&err), "a DNS blip is not an SSRF refusal");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a transient lookup failure must use the retry budget"
        );
    }

    #[test]
    fn only_a_definitive_lookup_answer_counts_as_no_such_host() {
        let gai = |detail: &str| {
            std::io::Error::other(format!("failed to lookup address information: {detail}"))
        };
        for detail in [
            "Name or service not known",
            "No address associated with hostname",
            "Name does not resolve",
            "Name has no usable address",
            "nodename nor servname provided, or not known",
        ] {
            assert!(is_no_such_host(&gai(detail)), "{detail}");
        }
        for detail in [
            "Temporary failure in name resolution",
            "Try again",
            "Non-recoverable failure in name resolution",
            "Name ou service inconnu", // a translated glibc message: retried, not final
        ] {
            assert!(!is_no_such_host(&gai(detail)), "{detail}");
        }
        assert!(!is_no_such_host(&std::io::Error::from(
            std::io::ErrorKind::TimedOut
        )));
    }

    /// A one-shot HTTP server on 127.0.0.1 that answers its first request with a 302 to
    /// `location(port)`. Returns the listener (to check for later connections), its port, and a
    /// handle yielding the request line the server received.
    fn redirect_once(
        location: impl FnOnce(u16) -> String,
    ) -> (std::net::TcpListener, u16, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let location = location(port);
        let server = listener.try_clone().unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = server.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            write!(
                &stream,
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            )
            .unwrap();
            request_line
        });
        (listener, port, handle)
    }

    #[tokio::test]
    async fn guarded_client_refuses_a_redirect_hop_that_resolves_to_a_blocked_address() {
        // The first hop is an IP literal, so it skips DNS and lands on the local server (the
        // up-front `validate_public_url_str` that `pack add-url` runs is deliberately skipped
        // here). Its 302 names a hostname the rebinding DNS answers with loopback.
        let (listener, port, server) =
            redirect_once(|port| format!("http://internal.test:{port}/"));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client =
            ssrf_guarded_client_with_lookup(5, stub_lookup(vec![vec!["127.0.0.1"]], calls.clone()));
        let url = format!("http://127.0.0.1:{port}/start");
        let err = send_with_retry(|| client.get(&url), 3).await.unwrap_err();
        assert!(server.join().unwrap().starts_with("GET /start "));
        assert!(
            is_ssrf_block(&err),
            "expected an SSRF refusal, got: {err:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the hop is resolved once and the refusal is not retried"
        );
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the guarded client must never connect to the redirect hop's private address"
        );
    }

    #[tokio::test]
    async fn guarded_client_refuses_a_redirect_hop_to_an_alternate_form_ip_literal() {
        // `0x7f.1` is 127.0.0.1: the redirect policy refuses it without any DNS lookup.
        let (listener, port, server) = redirect_once(|port| format!("http://0x7f.1:{port}/"));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client = ssrf_guarded_client_with_lookup(5, stub_lookup(vec![vec![]], calls.clone()));
        let url = format!("http://127.0.0.1:{port}/start");
        let err = send_with_retry(|| client.get(&url), 3).await.unwrap_err();
        assert!(server.join().unwrap().starts_with("GET /start "));
        assert!(
            is_ssrf_block(&err),
            "expected an SSRF refusal, got: {err:?}"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
        );
    }

    #[test]
    fn http_client_with_user_agent_sets_the_given_header() {
        let client = http_client_with_user_agent(15, "indexa/9.9.9").expect("build client");
        // reqwest merges a client's default headers (incl. `user_agent()`) into a
        // request only at send time (`execute_request`), not when a `RequestBuilder`
        // is merely `.build()`'t — so there's no way to observe them on an unsent
        // request. `Client`'s `Debug` impl does print its configured default headers,
        // so use that as the observable proxy instead of standing up a live listener.
        let debug = format!("{client:?}");
        assert!(
            debug.contains("indexa/9.9.9"),
            "client Debug output should show the configured User-Agent, got: {debug}"
        );
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
