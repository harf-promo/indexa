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
        // Generic `api_key = "…"` / `secret: '…'` / `token=…` assignments (value redacted).
        mk(
            "assignment",
            r#"(?i)\b(api[_-]?key|secret|token|password|passwd|access[_-]?key)\b\s*[:=]\s*['"]?[A-Za-z0-9_\-./+]{12,}['"]?"#,
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
                replaced.push_str(&out[last..whole.start()]);
                replaced.push_str(&format!("{keyname} = [REDACTED-secret]"));
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

#[cfg(test)]
mod tests {
    use super::redact_secrets;

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
    fn assignment_keeps_key_name_redacts_value() {
        let (out, n) = redact_secrets("password = hunter2supersecretvalue");
        assert_eq!(n, 1);
        assert!(
            out.starts_with("password = [REDACTED-secret]"),
            "got: {out}"
        );
        assert!(!out.contains("hunter2supersecretvalue"));
    }
}
