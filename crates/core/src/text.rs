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

/// Human-readable byte size (binary units, 1 decimal): `512 B`, `4.2 KB`, `1.8 MB`, `3.0 GB`.
/// Single source of truth for byte formatting shared across surfaces (the usage savings line,
/// the per-answer impact readout) so a displayed number can't drift between them.
pub fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
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

    #[test]
    fn human_bytes_picks_the_right_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1_024), "1.0 KB");
        assert_eq!(human_bytes(1_536), "1.5 KB");
        assert_eq!(human_bytes(1_048_576), "1.0 MB");
        assert_eq!(human_bytes(1_073_741_824), "1.0 GB");
        assert_eq!(human_bytes(0), "0 B");
    }
}
