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

/// Escape a string for an XML **attribute** value: `&`, `"`, `<`, `>` (the `&`
/// replacement must run first). Single source of truth for the several pack/export
/// surfaces (CLI, web, MCP) that emit the same `<context pack="…">` / `<file path="…">`
/// XML, so a missed character can't drift between them.
pub fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string for XML **element text**: `&`, `<`, `>` (a literal `"` is legal in
/// text content, so it is left intact — the deliberate attr/text distinction).
pub fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Human-readable count with K / M suffixes (1 decimal): `42`, `1.4K`, `3.7M`.
/// Single source of truth for the ≈4-bytes/token estimate used by the savings line
/// (`UsageSummary::savings_line`), per-answer `AnswerImpact::human()`, and the web
/// Impact dashboard — one formula, no drift.
pub fn human_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Whether `bytes` look **binary**: a NUL byte in the first 8 KB — ripgrep's `is_binary`
/// heuristic. Cheap and reliable: real text (any encoding) essentially never contains a NUL,
/// so this catches executables, images, archives, and DB blobs without a full UTF-8 scan. The
/// caller passes the leading bytes it already read; this never opens a file. Single source of
/// truth shared by the scan walker's binary filter and the web file-preview `binary` flag.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Largest byte index ≤ `byte` that is a UTF-8 char boundary of `s` (clamped to
/// `s.len()`). A stable stand-in for the nightly-only `str::floor_char_boundary`,
/// so byte-budget truncation (`&s[..floor_char_boundary(s, n)]`) never slices
/// mid-codepoint — slicing a `str` at a raw byte offset panics on any multibyte
/// content (accents, CJK, emoji, em-dashes).
pub fn floor_char_boundary(s: &str, byte: usize) -> usize {
    let mut b = byte.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_attr_covers_all_four() {
        assert_eq!(
            xml_escape_attr(r#"a & b < c > d " e"#),
            "a &amp; b &lt; c &gt; d &quot; e"
        );
    }

    #[test]
    fn is_binary_detects_nul_only() {
        assert!(is_binary(b"abc\0def"), "a NUL byte marks binary");
        assert!(is_binary(&[0u8, 1, 2]), "leading NUL marks binary");
        assert!(!is_binary(b"plain ascii text"), "text has no NUL");
        assert!(
            !is_binary("héllo — utf8 ☺".as_bytes()),
            "UTF-8 text is not binary"
        );
        assert!(!is_binary(b""), "empty is not binary");
        // Only the first 8 KB matter: a NUL past the window is not sniffed.
        let mut late = vec![b'a'; 9000];
        late.push(0);
        assert!(!is_binary(&late), "NUL past the 8 KB window is ignored");
    }

    #[test]
    fn xml_escape_text_leaves_quotes() {
        // Text content keeps `"` (only `& < >` are special there).
        assert_eq!(
            xml_escape_text(r#"x < y > z & "q""#),
            "x &lt; y &gt; z &amp; \"q\""
        );
    }

    #[test]
    fn floor_char_boundary_clamps_and_respects_codepoints() {
        assert_eq!(floor_char_boundary("hello", 3), 3); // ASCII boundary
        assert_eq!(floor_char_boundary("hello", 99), 5); // clamps to len
        assert_eq!(floor_char_boundary("café", 0), 0); // zero
        let s = "café";
        assert_eq!(s.len(), 5); // precomposed é (U+00E9) is 2 bytes: c a f é = 5 bytes
        assert_eq!(floor_char_boundary(s, 4), 3); // byte 4 is mid-é → walk back to 3
        assert_eq!(floor_char_boundary(s, 5), 5); // byte 5 == len, already a boundary
        assert!(s.is_char_boundary(floor_char_boundary(s, 4)));
        // CJK: each char 3 bytes; byte 4 → walk back to 3.
        let cjk = "日本語";
        assert_eq!(floor_char_boundary(cjk, 4), 3);
        assert!(cjk.is_char_boundary(floor_char_boundary(cjk, 4)));
    }

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
