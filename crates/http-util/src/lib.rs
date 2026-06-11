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

#[cfg(test)]
mod tests {
    use super::*;

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
