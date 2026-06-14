//! Opt-in remote-source ingestion for Context Packs (v0.32).
//!
//! `indexa pack add-url` fetches a **GitHub issue/PR** (via the public API — already Markdown) or a
//! **web page** (HTML → Markdown), converts it to Markdown, and caches it as a local file under the
//! data dir so it flows through the normal index pipeline (scan → deep → summarize → export). It
//! reaches the network, so it's gated behind `[sources] enabled` or `INDEXA_REMOTE_FETCH_ALLOW=1`.
//!
//! Scope is deliberately narrow: GitHub + generic web only. arXiv/YouTube etc. belong in optional
//! Plugin-SDK parsers, not core, because such scrapers rot (onefilellm's own README admits this).

use anyhow::{Context, Result};
use indexa_core::config::SourcesConfig;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const UA: &str = concat!("indexa/", env!("CARGO_PKG_VERSION"));

/// Whether remote fetching is permitted (config flag OR per-run env override).
pub(crate) fn remote_fetch_allowed(cfg: &SourcesConfig) -> bool {
    cfg.enabled || std::env::var("INDEXA_REMOTE_FETCH_ALLOW").as_deref() == Ok("1")
}

/// A GitHub issue/PR coordinate parsed from a URL.
struct GhRef {
    owner: String,
    repo: String,
    number: u64,
}

/// Parse `https://github.com/{owner}/{repo}/(issues|pull)/{n}` into coordinates.
fn parse_github_issue_or_pr(url: &str) -> Option<GhRef> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() >= 4 && (parts[2] == "issues" || parts[2] == "pull") {
        let number = parts[3].split(['?', '#']).next()?.parse().ok()?;
        return Some(GhRef {
            owner: parts[0].to_owned(),
            repo: parts[1].to_owned(),
            number,
        });
    }
    None
}

/// Fetch `url` and convert to Markdown, dispatching GitHub issue/PR URLs to the API path.
pub(crate) async fn fetch_source_markdown(url: &str, cfg: &SourcesConfig) -> Result<String> {
    if let Some(gh) = parse_github_issue_or_pr(url) {
        fetch_github(&gh, cfg.timeout_secs, cfg.max_retries).await
    } else {
        fetch_web(url, cfg.timeout_secs, cfg.max_retries).await
    }
}

/// Fetch a GitHub issue/PR (title, meta, body, comments) as Markdown via the public API. Uses
/// `GITHUB_TOKEN` if present (higher rate limit); works unauthenticated otherwise.
async fn fetch_github(gh: &GhRef, timeout: u64, retries: u32) -> Result<String> {
    let client = indexa_http_util::http_client(timeout);
    let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
    let api = format!(
        "https://api.github.com/repos/{}/{}/issues/{}",
        gh.owner, gh.repo, gh.number
    );
    let get = |u: &str| {
        let mut rb = client
            .get(u)
            .header("User-Agent", UA)
            .header("Accept", "application/vnd.github+json");
        if let Some(t) = &token {
            rb = rb.bearer_auth(t);
        }
        rb
    };

    let resp = indexa_http_util::send_with_retry(|| get(&api), retries)
        .await
        .context("GitHub API request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned HTTP {} for {api}", resp.status());
    }
    let issue: serde_json::Value = resp.json().await.context("parsing GitHub issue JSON")?;
    let title = issue["title"].as_str().unwrap_or("(untitled)");
    let state = issue["state"].as_str().unwrap_or("");
    let user = issue["user"]["login"].as_str().unwrap_or("");
    let body = issue["body"].as_str().unwrap_or("");
    let mut md = format!(
        "# {title}\n\n_GitHub {}/{} #{} · {state} · @{user}_\n\n{body}\n",
        gh.owner, gh.repo, gh.number
    );

    // Comments (best-effort — never fail the whole fetch over them).
    if let Some(curl) = issue["comments_url"].as_str() {
        if let Ok(cresp) = indexa_http_util::send_with_retry(|| get(curl), retries).await {
            if cresp.status().is_success() {
                if let Ok(comments) = cresp.json::<Vec<serde_json::Value>>().await {
                    if !comments.is_empty() {
                        md.push_str("\n## Comments\n");
                        for c in &comments {
                            let cu = c["user"]["login"].as_str().unwrap_or("");
                            let cb = c["body"].as_str().unwrap_or("");
                            md.push_str(&format!("\n**@{cu}:**\n\n{cb}\n"));
                        }
                    }
                }
            }
        }
    }
    Ok(md)
}

/// Fetch an arbitrary web page and convert it to Markdown (best-effort).
async fn fetch_web(url: &str, timeout: u64, retries: u32) -> Result<String> {
    let client = indexa_http_util::http_client(timeout);
    let resp =
        indexa_http_util::send_with_retry(|| client.get(url).header("User-Agent", UA), retries)
            .await
            .context("web fetch failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} fetching {url}", resp.status());
    }
    let html = resp.text().await.context("reading response body")?;
    // Drop <script>/<style> blocks first so their JS/CSS can't leak into the Markdown.
    let cleaned = strip_blocks(&strip_blocks(&html, "script"), "style");
    let md = htmd::convert(&cleaned).context("converting HTML to Markdown")?;
    if md.trim().is_empty() {
        anyhow::bail!("converted page was empty — not HTML, or no extractable text");
    }
    Ok(md)
}

/// Remove every `<tag …>…</tag>` block (case-insensitive) from `html`. Used to drop `<script>`
/// and `<style>` before HTML→Markdown so their bodies don't leak into the output. Byte-offset safe:
/// `to_ascii_lowercase` only changes ASCII case, preserving length + char boundaries.
fn strip_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        match lower[i..].find(&open) {
            Some(rel) => {
                let start = i + rel;
                out.push_str(&html[i..start]);
                match lower[start..].find(&close) {
                    Some(crel) => i = start + crel + close.len(),
                    None => break, // unterminated — drop the remainder
                }
            }
            None => {
                out.push_str(&html[i..]);
                break;
            }
        }
    }
    out
}

/// Write fetched Markdown to `<data_dir>/sources/<slug>-<sha8>.md` with a provenance header.
/// Returns the cache-file path. `label` overrides the URL-derived slug. The filename keys on the
/// URL hash, so re-fetching the same URL overwrites in place (no duplicate cache files).
pub(crate) fn cache_source(
    data_dir: &Path,
    url: &str,
    label: Option<&str>,
    content: &str,
) -> Result<PathBuf> {
    let dir = data_dir.join("sources");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let sha = format!("{:x}", Sha256::digest(url.as_bytes()));
    let slug = label.map(slugify).unwrap_or_else(|| slug_from_url(url));
    let path = dir.join(format!("{slug}-{}.md", &sha[..8]));
    let body = format!("<!-- indexa remote source: {url} -->\n\n{content}");
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Lowercase, keep alphanumerics, collapse the rest to single dashes, cap at 48 chars.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(48));
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        "source".to_owned()
    } else {
        trimmed
    }
}

/// Derive a slug from a URL's host + path (drops the scheme).
fn slug_from_url(url: &str) -> String {
    let no_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    slugify(no_scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_issue_and_pr_urls() {
        let i =
            parse_github_issue_or_pr("https://github.com/harf-promo/indexa/issues/219").unwrap();
        assert_eq!(
            (i.owner.as_str(), i.repo.as_str(), i.number),
            ("harf-promo", "indexa", 219)
        );
        let p =
            parse_github_issue_or_pr("https://github.com/rust-lang/rust/pull/12345?foo=1").unwrap();
        assert_eq!(
            (p.owner.as_str(), p.repo.as_str(), p.number),
            ("rust-lang", "rust", 12345)
        );
        // Non-issue GitHub URLs and other hosts are not GitHub-issue refs.
        assert!(parse_github_issue_or_pr("https://github.com/harf-promo/indexa").is_none());
        assert!(parse_github_issue_or_pr("https://example.com/issues/1").is_none());
    }

    #[test]
    fn strip_blocks_removes_script_and_style() {
        let html =
            "<p>keep</p><style>body{color:red}</style><div>also</div><SCRIPT>evil()</SCRIPT>end";
        let cleaned = strip_blocks(&strip_blocks(html, "script"), "style");
        assert!(!cleaned.contains("color:red"), "css leaked: {cleaned}");
        assert!(!cleaned.contains("evil()"), "js leaked: {cleaned}");
        assert!(cleaned.contains("keep") && cleaned.contains("also") && cleaned.contains("end"));
    }

    #[test]
    fn slug_from_url_is_filesystem_safe() {
        let s = slug_from_url("https://docs.rs/serde/latest/serde/index.html");
        assert!(!s.contains('/') && !s.contains(':') && !s.contains('.'));
        assert!(s.starts_with("docs-rs-serde"));
        assert!(s.len() <= 48);
        assert_eq!(slugify("!!!"), "source"); // degenerate input never yields an empty name
    }
}
