//! Secret scanning + redaction for exported content.
//!
//! Indexa's `export`/`pack export` normally render model-written *summaries* (low secret risk), but
//! the v0.31 `--signatures` mode emits real source lines, and a summary could still echo a config
//! value. Before any export leaves the machine we run [`redact_secrets`], which replaces obvious
//! credentials with `[REDACTED-<kind>]` and reports how many it caught. Pattern-based and
//! conservative (well-known token shapes + private-key blocks + `key = "…"` assignments) — it is a
//! safety net, not a guarantee; we never claim more than it does.

use regex::Regex;
use std::sync::LazyLock;

/// A compiled secret pattern and the label used in its redaction marker.
struct Pattern {
    re: Regex,
    kind: &'static str,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let mk = |kind: &'static str, src: &str| Pattern {
        re: Regex::new(src).expect("static secret regex compiles"),
        kind,
    };
    vec![
        // PEM private key blocks (RSA/EC/OPENSSH/generic), across newlines.
        mk(
            "private-key",
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        ),
        // AWS access key id.
        mk("aws-key", r"\bAKIA[0-9A-Z]{16}\b"),
        // GitHub tokens: ghp_/gho_/ghu_/ghs_/ghr_ + 36+ base62.
        mk("github-token", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
        // Slack tokens.
        mk("slack-token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"),
        // Google API key.
        mk("google-key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
        // OpenAI-style secret keys (sk-... / sk-proj-...).
        mk("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}\b"),
        // JSON Web Tokens: base64url `header.payload.signature`, always starting `eyJ` (b64 of `{"`).
        mk(
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        ),
        // Stripe secret/restricted keys (live+test) and webhook signing secrets.
        mk(
            "stripe-key",
            r"\b(?:[sr]k_(?:live|test)|whsec)_[A-Za-z0-9]{10,}\b",
        ),
        // GitLab personal/project/group access tokens.
        mk("gitlab-token", r"\bglpat-[A-Za-z0-9_-]{20,}\b"),
        // npm access tokens.
        mk("npm-token", r"\bnpm_[A-Za-z0-9]{30,}\b"),
        // SendGrid API keys (`SG.<id>.<secret>`).
        mk(
            "sendgrid-key",
            r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\b",
        ),
        // Google OAuth access tokens.
        mk("google-oauth", r"\bya29\.[A-Za-z0-9_-]{20,}"),
        // Credentials embedded in a URL (`scheme://user:pass@host…`) — the whole match is redacted
        // (a creds-bearing URL host is itself often sensitive). Raw `r#"…"#` for the `"` in the
        // terminating char class.
        mk(
            "url-credentials",
            r#"(?i)\b[a-z][a-z0-9+.-]*://[^/\s:@]+:[^/\s:@]+@[^\s"'<>)\]]+"#,
        ),
        // Generic `api_key = "…"` / `secret: '…'` / `token=…` assignments (value redacted).
        // Group 1 = key name, group 2 = the separator (`: ` / ` = ` / `=`) so it's preserved in
        // the output — redacting a YAML/TOML config keeps it syntactically intact.
        mk(
            "assignment",
            r#"(?i)\b(api[_-]?key|secret|token|password|passwd|access[_-]?key)\b(\s*[:=]\s*)['"]?[A-Za-z0-9_\-./+]{12,}['"]?"#,
        ),
    ]
});

/// Redact obvious secrets in `input`. Returns the cleaned string and the number of redactions.
///
/// For the `assignment` pattern the *key name* is preserved and only the value is replaced
/// (`api_key = [REDACTED-assignment]`), so the export stays readable; all other patterns replace
/// the whole match with `[REDACTED-<kind>]`.
pub fn redact_secrets(input: &str) -> (String, usize) {
    let mut out = input.to_owned();
    let mut count = 0usize;
    for p in PATTERNS.iter() {
        if p.kind == "assignment" {
            // Keep the key + separator, redact the value.
            let mut replaced = String::with_capacity(out.len());
            let mut last = 0;
            for caps in p.re.captures_iter(&out) {
                let whole = caps.get(0).unwrap();
                let keyname = caps.get(1).map(|m| m.as_str()).unwrap_or("key");
                // Preserve the original separator (`: ` / ` = ` / `=`) so a redacted config
                // keeps its source syntax instead of being normalized to ` = `.
                let sep = caps.get(2).map(|m| m.as_str()).unwrap_or(" = ");
                replaced.push_str(&out[last..whole.start()]);
                replaced.push_str(&format!("{keyname}{sep}[REDACTED-secret]"));
                last = whole.end();
                count += 1;
            }
            replaced.push_str(&out[last..]);
            out = replaced;
        } else {
            let marker = format!("[REDACTED-{}]", p.kind);
            let n = p.re.find_iter(&out).count();
            if n > 0 {
                out = p.re.replace_all(&out, marker.as_str()).into_owned();
                count += n;
            }
        }
    }
    (out, count)
}

/// The chunk text to STORE (it becomes searchable via FTS/embeddings and exportable) — with
/// secrets redacted when `redact` is set. This is the single choke point every index write path
/// (CLI/web `deep`, CLI/web `watch`) routes through so `[scan] redact_at_index` (default on) can't
/// be silently bypassed on one surface. The embedding and `content_hash` are always computed over
/// the ORIGINAL text (keeping the embed cache stable), so only what lands in the store is scrubbed.
pub fn chunk_text_for_store(text: &str, redact: bool) -> String {
    if redact {
        redact_secrets(text).0
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{chunk_text_for_store, redact_secrets};

    #[test]
    fn chunk_text_for_store_redacts_only_when_enabled() {
        let secret = "aws key AKIAIOSFODNN7EXAMPLE in source";
        // Enabled → the key is scrubbed from what gets stored/searched.
        let stored = chunk_text_for_store(secret, true);
        assert!(!stored.contains("AKIAIOSFODNN7EXAMPLE"), "stored: {stored}");
        assert!(stored.contains("[REDACTED"), "stored: {stored}");
        // Disabled → verbatim.
        assert_eq!(chunk_text_for_store(secret, false), secret);
    }

    #[test]
    fn redacts_aws_and_github_and_private_key_but_keeps_prose() {
        let input = "\
The retrieval pipeline fuses BM25 and vectors via RRF.
aws_key = AKIAIOSFODNN7EXAMPLE
token: ghp_0123456789abcdefABCDEF0123456789abcd
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAabcdef
-----END RSA PRIVATE KEY-----
This sentence is ordinary documentation and must survive.";
        let (out, n) = redact_secrets(input);
        assert!(n >= 3, "expected >=3 redactions, got {n}");
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "AWS key leaked");
        assert!(
            !out.contains("ghp_0123456789abcdefABCDEF"),
            "GitHub token leaked"
        );
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "private key body leaked");
        assert!(out.contains("[REDACTED-aws-key]"));
        assert!(out.contains("[REDACTED-private-key]"));
        // Prose is untouched.
        assert!(out.contains("fuses BM25 and vectors via RRF"));
        assert!(out.contains("ordinary documentation and must survive"));
    }

    #[test]
    fn clean_text_is_unchanged_and_zero_count() {
        let input = "fn retrieve() -> Result<Vec<Hit>> { hybrid_search() }";
        let (out, n) = redact_secrets(input);
        assert_eq!(n, 0);
        assert_eq!(out, input);
    }

    #[test]
    fn redacts_expanded_token_families() {
        // Each fixture is assembled from split literals at runtime so the SOURCE never contains a
        // contiguous provider-shaped token — else GitHub push protection blocks the commit. Joined,
        // they still exercise the real patterns.
        let j = |a: &str, b: &str| format!("{a}{b}");
        let cases = [
            (
                "jwt",
                j(
                    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM",
                    "0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                ),
            ),
            ("stripe secret", j("sk_live", "_0123456789abcdefABCDEF01")),
            ("stripe webhook", j("whsec", "_0123456789abcdefABCDEF01")),
            ("gitlab", j("glpat", "-0123456789abcdefABCD")),
            ("npm", j("npm", "_0123456789abcdefABCDEF0123456789abcd")),
            (
                "sendgrid",
                j(
                    "SG",
                    ".0123456789abcdefABCDEF.0123456789abcdefABCDEF0123456789abcd",
                ),
            ),
            (
                "google oauth",
                j("ya29", ".0123456789abcdefABCDEF0123456789"),
            ),
        ];
        for (label, secret) in &cases {
            // `value` is not an assignment keyword, so only the specific token pattern can catch it.
            let input = format!("cfg value = {secret} end");
            let (out, n) = redact_secrets(&input);
            assert!(n >= 1, "{label}: no redaction in `{input}`");
            assert!(
                !out.contains(secret.as_str()),
                "{label}: secret leaked: `{out}`"
            );
            assert!(out.contains("[REDACTED-"), "{label}: no marker: `{out}`");
        }
    }

    #[test]
    fn redacts_url_embedded_credentials() {
        // Password kept as its own literal so the source has no contiguous `user:pass@` token.
        let input = format!(
            "clone from https://alice:{}@git.internal/repo.git today",
            "s3cr3tpassword"
        );
        let (out, n) = redact_secrets(&input);
        assert_eq!(n, 1, "got: {out}");
        assert!(!out.contains("s3cr3tpassword"), "password leaked: {out}");
        assert!(out.contains("[REDACTED-url-credentials]"), "got: {out}");
        // Surrounding prose survives.
        assert!(
            out.contains("clone from") && out.contains("today"),
            "got: {out}"
        );
    }

    #[test]
    fn assignment_keeps_key_name_redacts_value() {
        let (out, n) = redact_secrets("password = hunter2supersecretvalue");
        assert_eq!(n, 1);
        assert!(
            out.starts_with("password = [REDACTED-secret]"),
            "got: {out}"
        );
        assert!(!out.contains("hunter2supersecretvalue"));
    }

    #[test]
    fn assignment_preserves_the_original_separator() {
        // Colon separator (YAML-style) stays a colon, not normalized to ` = `, so a redacted
        // config keeps its source syntax. The secret value is still removed.
        let (out, n) = redact_secrets("api_key: verylongsecretvalue123");
        assert_eq!(n, 1);
        assert_eq!(out, "api_key: [REDACTED-secret]", "got: {out}");
        assert!(!out.contains("verylongsecretvalue123"));

        // No-space `=` stays tight.
        let (out2, _) = redact_secrets("token=verylongsecretvalue123");
        assert_eq!(out2, "token=[REDACTED-secret]", "got: {out2}");
    }
}
