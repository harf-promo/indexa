//! Predicate micro-grammar for free-text search/ask queries (1.8) — borrowed from
//! GoogleCloudPlatform/knowledge-catalog's discovery `SKILL.md`: `field:value` tokens parsed
//! out of a query and mapped onto filters Indexa already has, so `ext:md auth flow` searches
//! for "auth flow" scoped to `.md` files without a new tool param.
//!
//! Known fields (extend [`KNOWN_FIELDS`] to add more):
//! - `path:<prefix>` — restrict to files under this path prefix (maps onto the existing
//!   `scope` config already used by `search`/`ask`).
//! - `ext:<extension>` — restrict to files with this extension (a post-hoc hit filter — no
//!   extra store round-trip, no schema change).
//! - `type:<name>` — ripgrep-style named file-type set (`type:python` == `.py`/`.pyi`), a
//!   curated multi-extension convenience over `ext:` — see [`TYPE_SETS`]. Unlike `path`/`ext`
//!   (open-ended, any value accepted), `type` has a closed vocabulary: a value that isn't a
//!   known set name is **not** consumed as a predicate (falls through as plain text), matching
//!   `ext`/`path`'s safety property for an unrecognized *field* but applied to `type`'s
//!   unrecognized *values* instead.
//!
//! Only a token whose field is in [`KNOWN_FIELDS`] (and, for `type`, whose value is a known
//! set name) is consumed. Everything else — plain words, unrecognized `field:value` shapes
//! (`http://…`, `note:` with nothing after it), empty values — passes through in
//! [`ParsedQuery::text`] unchanged. This is the safety property that lets the grammar ship
//! default-off and expand later without breaking a query that merely happens to contain a
//! colon.

/// Fields this grammar recognizes as predicates. A `field:value` token whose field is NOT
/// in this list is left as ordinary query text (never silently eaten).
const KNOWN_FIELDS: &[&str] = &["path", "ext", "type"];

/// Curated named file-type sets (ripgrep's `--type`, narrowed to what's useful for a
/// code-context tool rather than all ~200 of ripgrep's built-ins). Extend freely — an
/// unrecognized name simply isn't treated as a predicate (see module docs), so adding entries
/// here is purely additive and never changes behavior for a name not yet listed.
const TYPE_SETS: &[(&str, &[&str])] = &[
    ("rust", &["rs"]),
    ("python", &["py", "pyi"]),
    ("js", &["js", "jsx", "mjs", "cjs"]),
    ("ts", &["ts", "tsx"]),
    ("go", &["go"]),
    ("java", &["java"]),
    ("c", &["c", "h"]),
    ("cpp", &["cpp", "cc", "cxx", "hpp", "hh", "hxx"]),
    ("web", &["html", "css", "scss", "less"]),
    ("docs", &["md", "mdx", "rst", "txt"]),
    ("config", &["toml", "yaml", "yml", "json"]),
    ("shell", &["sh", "bash", "zsh"]),
    ("ruby", &["rb"]),
    ("php", &["php"]),
    ("swift", &["swift"]),
    ("kotlin", &["kt", "kts"]),
];

/// Resolve a `type:<name>` value to its extension set (case-insensitive), or `None` when
/// `name` isn't a known set — the caller treats `None` as "not a predicate after all".
fn type_set_extensions(name: &str) -> Option<Vec<String>> {
    TYPE_SETS
        .iter()
        .find(|(set_name, _)| set_name.eq_ignore_ascii_case(name))
        .map(|(_, exts)| exts.iter().map(|e| e.to_string()).collect())
}

/// The result of stripping recognized predicates out of a free-text query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// The query with every recognized predicate token removed, whitespace-normalized.
    pub text: String,
    /// `path:<prefix>` — the last occurrence wins if given more than once.
    pub path: Option<String>,
    /// `ext:<extension>` — the leading `.` is stripped if present (`ext:.md` == `ext:md`).
    pub ext: Option<String>,
    /// `type:<name>` resolved to its extension set via [`TYPE_SETS`] — the last *recognized*
    /// occurrence wins if given more than once; an unrecognized name never sets this (see
    /// module docs).
    pub type_exts: Option<Vec<String>>,
}

/// Parse `query`, extracting any `path:`/`ext:`/`type:` predicates (see module docs). Splits
/// on whitespace, so a predicate value cannot itself contain a space (`path:"a b"` is not
/// supported — quote the whole query at the CLI/tool-call layer if that's ever needed).
pub fn parse_predicates(query: &str) -> ParsedQuery {
    let mut out = ParsedQuery::default();
    let mut remaining: Vec<&str> = Vec::new();
    for token in query.split_whitespace() {
        match split_predicate(token) {
            Some(("path", value)) => out.path = Some(value.to_owned()),
            Some(("ext", value)) => out.ext = Some(value.trim_start_matches('.').to_owned()),
            Some(("type", value)) => match type_set_extensions(value) {
                Some(exts) => out.type_exts = Some(exts),
                None => remaining.push(token), // unrecognized set name — not a predicate
            },
            _ => remaining.push(token),
        }
    }
    out.text = remaining.join(" ");
    out
}

/// Split `token` into `(field, value)` on the first `:` or `=`, only when `field` is a
/// [`KNOWN_FIELDS`] member and `value` is non-empty. Returns `None` for ordinary words, an
/// unrecognized field, or an empty value (`"note:"`) — the caller keeps the token as-is.
fn split_predicate(token: &str) -> Option<(&str, &str)> {
    let idx = token.find([':', '='])?;
    let (field, rest) = token.split_at(idx);
    let value = &rest[1..];
    if value.is_empty() || !KNOWN_FIELDS.contains(&field) {
        return None;
    }
    Some((field, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_known_predicates_and_leaves_the_rest() {
        let p = parse_predicates("ext:md path:crates/core auth flow");
        assert_eq!(p.ext, Some("md".to_owned()));
        assert_eq!(p.path, Some("crates/core".to_owned()));
        assert_eq!(p.text, "auth flow");
    }

    #[test]
    fn strips_a_leading_dot_on_ext() {
        assert_eq!(parse_predicates("ext:.md x").ext, Some("md".to_owned()));
    }

    #[test]
    fn plain_query_with_no_predicates_is_unchanged() {
        let p = parse_predicates("how does retrieval work");
        assert_eq!(p.text, "how does retrieval work");
        assert!(p.path.is_none());
        assert!(p.ext.is_none());
    }

    #[test]
    fn unknown_field_shaped_tokens_pass_through_as_text() {
        // A colon-bearing token whose field isn't in KNOWN_FIELDS must never be eaten —
        // this is what lets the grammar expand later without breaking existing queries.
        let p = parse_predicates("lang:rust pack:auth see http://example.com");
        assert_eq!(p.text, "lang:rust pack:auth see http://example.com");
        assert!(p.path.is_none());
        assert!(p.ext.is_none());
        assert!(p.type_exts.is_none());
    }

    #[test]
    fn type_expands_a_known_set_to_its_extensions() {
        let p = parse_predicates("type:python auth flow");
        assert_eq!(p.type_exts, Some(vec!["py".to_owned(), "pyi".to_owned()]));
        assert_eq!(p.text, "auth flow");
    }

    #[test]
    fn type_is_case_insensitive() {
        assert_eq!(
            parse_predicates("type:PYTHON x").type_exts,
            Some(vec!["py".to_owned(), "pyi".to_owned()])
        );
    }

    #[test]
    fn an_unrecognized_type_name_is_not_a_predicate() {
        // `type` IS a known field, but its value has a closed vocabulary (unlike ext/path) —
        // an unrecognized set name must fall through as plain text, never silently swallowed.
        let p = parse_predicates("type:bogus auth flow");
        assert!(p.type_exts.is_none());
        assert_eq!(p.text, "type:bogus auth flow");
    }

    #[test]
    fn type_and_ext_and_path_combine() {
        let p = parse_predicates("type:rust ext:rs path:crates/core auth");
        assert_eq!(p.type_exts, Some(vec!["rs".to_owned()]));
        assert_eq!(p.ext, Some("rs".to_owned()));
        assert_eq!(p.path, Some("crates/core".to_owned()));
        assert_eq!(p.text, "auth");
    }

    #[test]
    fn later_type_predicate_wins_on_repeat_but_only_if_recognized() {
        let p = parse_predicates("type:rust type:python x");
        assert_eq!(p.type_exts, Some(vec!["py".to_owned(), "pyi".to_owned()]));
        // A later unrecognized value does NOT clobber an earlier recognized one — it just
        // isn't consumed as a predicate at all, so it falls through as text instead.
        let p2 = parse_predicates("type:rust type:bogus x");
        assert_eq!(p2.type_exts, Some(vec!["rs".to_owned()]));
        assert_eq!(p2.text, "type:bogus x");
    }

    #[test]
    fn a_colon_with_nothing_after_it_is_not_a_predicate() {
        let p = parse_predicates("see path: below for details");
        assert_eq!(p.text, "see path: below for details");
        assert!(p.path.is_none());
    }

    #[test]
    fn empty_query_stays_empty() {
        let p = parse_predicates("");
        assert_eq!(p.text, "");
        assert!(p.path.is_none() && p.ext.is_none());
    }

    #[test]
    fn later_path_predicate_wins_on_repeat() {
        let p = parse_predicates("path:a path:b");
        assert_eq!(p.path, Some("b".to_owned()));
        assert_eq!(p.text, "");
    }

    #[test]
    fn equals_separator_is_also_recognized() {
        let p = parse_predicates("ext=md hello");
        assert_eq!(p.ext, Some("md".to_owned()));
        assert_eq!(p.text, "hello");
    }
}
