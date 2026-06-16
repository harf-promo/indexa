//! Text utilities shared across crates.

/// Truncate `s` to at most `n` Unicode characters, respecting char boundaries.
/// Returns the original string if it is already short enough.
pub fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Truncate `s` to at most `n` Unicode characters and append "…" if truncated.
pub fn snippet(s: &str, n: usize) -> std::borrow::Cow<'_, str> {
    match s.char_indices().nth(n) {
        Some((i, _)) => std::borrow::Cow::Owned(format!("{}…", &s[..i])),
        None => std::borrow::Cow::Borrowed(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_short_string_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn truncate_chars_clips_at_n_chars() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }

    #[test]
    fn truncate_chars_respects_multibyte() {
        // "café" = 4 chars, 5 bytes
        assert_eq!(truncate_chars("café world", 4), "café");
    }

    #[test]
    fn snippet_no_truncation_when_short() {
        let s = snippet("hi", 10);
        assert_eq!(s, "hi");
        assert!(matches!(s, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn snippet_appends_ellipsis_when_truncated() {
        let s = snippet("hello world", 5);
        assert_eq!(s, "hello…");
        assert!(matches!(s, std::borrow::Cow::Owned(_)));
    }

    #[test]
    fn snippet_multibyte_safe() {
        // "日本語" = 3 chars, 9 bytes — truncate at 2
        let s = snippet("日本語テスト", 2);
        assert_eq!(s, "日本…");
    }
}
