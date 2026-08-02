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
//!
//! Only a token whose field is in [`KNOWN_FIELDS`] is consumed. Everything else — plain
//! words, unrecognized `field:value` shapes (`http://…`, `note:` with nothing after it),
//! empty values — passes through in [`ParsedQuery::text`] unchanged. This is the safety
//! property that lets the grammar ship default-off and expand later without breaking a
//! query that merely happens to contain a colon.

/// Fields this grammar recognizes as predicates. A `field:value` token whose field is NOT
/// in this list is left as ordinary query text (never silently eaten).
const KNOWN_FIELDS: &[&str] = &["path", "ext"];

/// The result of stripping recognized predicates out of a free-text query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// The query with every recognized predicate token removed, whitespace-normalized.
    pub text: String,
    /// `path:<prefix>` — the last occurrence wins if given more than once.
    pub path: Option<String>,
    /// `ext:<extension>` — the leading `.` is stripped if present (`ext:.md` == `ext:md`).
    pub ext: Option<String>,
}

/// Parse `query`, extracting any `path:`/`ext:` predicates (see module docs). Splits on
/// whitespace, so a predicate value cannot itself contain a space (`path:"a b"` is not
/// supported — quote the whole query at the CLI/tool-call layer if that's ever needed).
pub fn parse_predicates(query: &str) -> ParsedQuery {
    let mut out = ParsedQuery::default();
    let mut remaining: Vec<&str> = Vec::new();
    for token in query.split_whitespace() {
        match split_predicate(token) {
            Some(("path", value)) => out.path = Some(value.to_owned()),
            Some(("ext", value)) => out.ext = Some(value.trim_start_matches('.').to_owned()),
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
        let p = parse_predicates("lang:rust type:code pack:auth see http://example.com");
        assert_eq!(
            p.text,
            "lang:rust type:code pack:auth see http://example.com"
        );
        assert!(p.path.is_none());
        assert!(p.ext.is_none());
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
