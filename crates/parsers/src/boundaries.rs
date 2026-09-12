//! Where one language calls another.
//!
//! A call graph that stops at the language boundary stops exactly where the interesting
//! questions start. A JavaScript file calls `fetch("/api/models/${id}/animation")`; a Python
//! file declares `@app.post("/api/models/{model_path:path}/animation")`. Both are indexed. Until
//! now nothing joined them, so *"what breaks if I rename this endpoint"* had no answer in either
//! direction.
//!
//! # Why raw text, not the AST
//!
//! Deliberate. A boundary **is a string** — a route path, an exported symbol name — and the
//! string is what has to match. Parsing tells you `save_animation` is a function; only the
//! literal tells you it answers `POST /api/models/*/animation`.
//!
//! # What this refuses to do
//!
//! It does not guess. A client call whose path is built by string concatenation at runtime is
//! recorded as **unresolved** rather than approximated, and an endpoint nothing calls is
//! reported as exactly that. Both are findings — an unmatched client call is usually a typo or
//! a deleted route, and surfacing it is worth more than a plausible-looking edge that is wrong.
//!
//! This module is pure functions over text: no store, no config, no I/O. Persistence and the
//! join live elsewhere.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Which side of a boundary a fragment sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Declares an endpoint, or exports a symbol across the boundary.
    Provides,
    /// Calls an endpoint, or imports a symbol from across the boundary.
    Consumes,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Provides => "provides",
            Side::Consumes => "consumes",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "provides" => Some(Side::Provides),
            "consumes" => Some(Side::Consumes),
            _ => None,
        }
    }
}

/// What kind of boundary is being crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundaryKind {
    /// An HTTP route: `@app.get("/api/x")` ↔ `fetch("/api/x")`.
    Http,
    /// A `wasm_bindgen` or `extern "C"` symbol crossing the FFI boundary.
    Ffi,
}

impl BoundaryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BoundaryKind::Http => "http",
            BoundaryKind::Ffi => "ffi",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "http" => Some(BoundaryKind::Http),
            "ffi" => Some(BoundaryKind::Ffi),
            _ => None,
        }
    }
}

/// One half of a cross-language link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    pub kind: BoundaryKind,
    pub side: Side,
    /// Normalized join key: `GET /api/models/*/animation`, or `ffi:autodetectchains`.
    /// **Empty** means unresolved — the path was built at runtime and was deliberately not
    /// guessed at.
    pub key: String,
    /// The HTTP method, when the source stated one. `None` means **unstated**, which is NOT the
    /// same as GET — a `fetch` whose options object is built elsewhere genuinely does not say.
    pub method: Option<String>,
    /// The path or symbol exactly as written. Every display path uses this, never [`Self::key`].
    pub raw: String,
    pub language: String,
    pub line: usize,
    /// The file this fragment was found in.
    pub path: String,
}

impl Boundary {
    /// True when the path could not be resolved to a comparable key.
    pub fn is_unresolved(&self) -> bool {
        self.key.is_empty()
    }

    /// How to show this boundary to a human.
    ///
    /// The folded FFI key (`ffi:autodetectchains`) is a matching artefact that appears nowhere
    /// in the source, so printing it makes the reader hunt for a name that does not exist. Show
    /// the spelling the source actually used.
    pub fn label(&self) -> String {
        match self.kind {
            BoundaryKind::Http => match &self.method {
                Some(m) => format!("{m} {}", self.raw),
                None => self.raw.clone(),
            },
            BoundaryKind::Ffi => format!("ffi:{}", self.raw),
        }
    }
}

struct Patterns {
    /// `(regex, method capture group, path capture group, side)`.
    http: Vec<(Regex, Option<usize>, usize, Side)>,
    /// Client-call heads only — the same receivers as the `## clients` entries in `http` above,
    /// but the pattern stops at the opening `(` instead of requiring a quote to follow. Used as
    /// a fallback when a call's first argument does not start with a literal, so `fetch(url)`
    /// and `axios.get(endpoint)` are still recorded (as unresolved) instead of vanishing. See
    /// `first_argument_expression` for how the argument text itself is recovered.
    /// `(regex, method capture group)`.
    http_client_head: Vec<(Regex, Option<usize>)>,
    wasm_export: Regex,
    ffi_export: Regex,
    exported_fn: Regex,
    wasm_import: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let r = |s: &str| Regex::new(s).expect("boundary pattern must compile");
        Patterns {
            http: vec![
                // ── servers ──
                // Python: FastAPI / Flask / Starlette decorators.
                (r(r#"@\w+\.(get|post|put|patch|delete|head|options|websocket)\s*\(\s*["']([^"']+)["']"#), Some(1), 2, Side::Provides),
                (r(r#"@\w+\.route\s*\(\s*["']([^"']+)["']"#), None, 1, Side::Provides),
                // Rust: Axum/Actix `.route("/x", get(h))`, Rocket/Actix attributes.
                (r(r#"\.route\s*\(\s*"([^"]+)"\s*,\s*(get|post|put|patch|delete)"#), Some(2), 1, Side::Provides),
                (r(r#"#\[(get|post|put|patch|delete)\s*\(\s*"([^"]+)"\s*\)\]"#), Some(1), 2, Side::Provides),
                // Express / Koa / Fastify. The RECEIVER NAME is the only thing separating a
                // server route from a client call — `app.get` declares, `axios.get` calls, and
                // the rest of the line is identical.
                //
                // The leading `(?:^|[^@\w.])` is not decoration: `\b` alone also matched
                // Python's `@app.get("/x")`, so every FastAPI decorator was captured twice —
                // once correctly as a Python route, once again as an Express one. Rust's regex
                // has no lookbehind, so the preceding character is excluded explicitly. It also
                // stops `myapp.get(...)` from being read as `app.get(...)`.
                (r(r#"(?:^|[^@\w.])(?:app|router|server|mux|api_router)\.(get|post|put|patch|delete|all|use)\s*\(\s*["'`]([^"'`]+)["'`]"#), Some(1), 2, Side::Provides),
                // Go net/http and chi.
                (r(r#"HandleFunc\s*\(\s*"([^"]+)""#), None, 1, Side::Provides),
                // Spring.
                (r(r#"@(Get|Post|Put|Patch|Delete|Request)Mapping\s*\(\s*(?:value\s*=\s*)?"([^"]+)""#), Some(1), 2, Side::Provides),
                // ASP.NET.
                (r(r#"\[Http(Get|Post|Put|Patch|Delete)\s*\(\s*"([^"]+)"\s*\)\]"#), Some(1), 2, Side::Provides),

                // ── clients ──
                (r(r#"\bfetch\s*\(\s*["'`]([^"'`]+)["'`]"#), None, 1, Side::Consumes),
                (r(r#"\b(?:axios|http|client|api)\.(get|post|put|patch|delete)\s*\(\s*["'`]([^"'`]+)["'`]"#), Some(1), 2, Side::Consumes),
                (r(r#"\brequests\.(get|post|put|patch|delete)\s*\(\s*["']([^"']+)["']"#), Some(1), 2, Side::Consumes),
                (r(r#"\bnew WebSocket\s*\(\s*["'`]([^"'`]+)["'`]"#), None, 1, Side::Consumes),
            ],
            http_client_head: vec![
                (r(r#"\bfetch\s*\("#), None),
                (r(r#"\b(?:axios|http|client|api)\.(get|post|put|patch|delete)\s*\("#), Some(1)),
                (r(r#"\brequests\.(get|post|put|patch|delete)\s*\("#), Some(1)),
                (r(r#"\bnew WebSocket\s*\("#), None),
            ],
            // `#[wasm_bindgen]` sits on its own line above the item, so the exported name is
            // found by looking ahead rather than on this line.
            wasm_export: r(r#"#\[wasm_bindgen"#),
            ffi_export: r(r#"extern\s+"C"\s+fn\s+(\w+)"#),
            exported_fn: r(r#"\bfn\s+(\w+)"#),
            // `import { solve_ik, init } from './avatar_ik_wasm.js'`. The specifier itself is
            // matched broadly here — whether it actually names a wasm/FFI module is decided in
            // code by `looks_like_a_wasm_or_ffi_module`, because a bare `ffi` substring check
            // inside the regex would also match ordinary files like `office.js`/`traffic.js`,
            // and Rust's regex crate has no lookaround to bound it inline.
            wasm_import: r(r#"import\s*\{([^}]+)\}\s*from\s*["'`]([^"'`]+)["'`]"#),
        }
    })
}

/// Join key for a symbol crossing the FFI boundary.
///
/// Rust exports `auto_detect_chains`; the JavaScript that calls it says `autoDetectChains`.
/// Neither side is wrong — wasm-bindgen renames snake_case to camelCase in the bindings it
/// generates, and hand-written shims follow the same convention. Matching the names literally
/// reports every export as uncalled **and** every caller as calling nothing: two lists of false
/// findings describing one working boundary.
///
/// So the key ignores case and underscores. The name as written is kept in
/// [`Boundary::raw`], because a reader still needs to see which spelling each side uses.
pub fn ffi_key(name: &str) -> String {
    format!("ffi:{}", name.replace('_', "").to_lowercase())
}

/// Reduce a URL to something both sides can agree on.
///
/// The server writes `/api/models/{model_path:path}/animation` and the client writes
/// `` `/api/models/${modelPath}/animation` ``. They describe the same endpoint and share no
/// path-parameter syntax at all, so every parameter segment collapses to `*`.
pub fn normalise_path(raw: &str) -> String {
    let s = raw.split(['?', '#']).next().unwrap_or(raw);
    // Drop scheme and host.
    let s = match s.find("://") {
        Some(i) => {
            let rest = &s[i + 3..];
            rest.find('/').map(|j| &rest[j..]).unwrap_or("/")
        }
        None => s,
    };
    let segments: Vec<String> = s
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            let is_param = seg.contains('{')      // FastAPI, Spring
                || seg.contains('<')              // Flask
                || seg.contains('$')              // JS template literal
                || seg.starts_with(':')           // Express, Rails
                || seg.contains('*')
                || seg.contains('+'); // encodeURIComponent(...) fragments
            if is_param {
                "*".to_owned()
            } else {
                seg.to_lowercase()
            }
        })
        .collect();
    format!("/{}", segments.join("/"))
}

/// Does a captured string plausibly name an HTTP path?
///
/// A route regex will happily capture a CSS selector, a MIME type, or an event name from a line
/// that merely looks similar. Those produce junk keys that can never match anything, so they are
/// rejected outright rather than stored as noise.
fn looks_like_a_path(raw: &str) -> bool {
    raw.starts_with('/') || raw.contains("://")
}

/// Does an import's module specifier actually name a wasm/FFI binding, or does it just
/// happen to contain the letters?
///
/// `wasm` and `_bg` rarely collide with an unrelated word, so a plain substring check is
/// enough for those. `ffi` is different: `office.js` and `traffic.js` both contain the
/// literal substring "ffi", and treating every import from those files as an FFI boundary
/// would drown the real ones the same way treating every named import as one would (see
/// [`an_ordinary_js_import_is_not_an_ffi_boundary`]). So `ffi` only counts when it is not
/// flanked by an ASCII letter on either side — `ffi.js`, `my_ffi.js` and `ffi_bridge.js`
/// still match, `office.js` and `traffic.js` don't. Rust's regex crate has no lookaround to
/// express that boundary inline, so it is checked here instead of in the pattern.
fn looks_like_a_wasm_or_ffi_module(spec: &str) -> bool {
    if spec.contains("wasm") || spec.contains("_bg") {
        return true;
    }
    let bytes = spec.as_bytes();
    spec.match_indices("ffi").any(|(i, m)| {
        let before_is_letter = i > 0 && bytes[i - 1].is_ascii_alphabetic();
        let after = i + m.len();
        let after_is_letter = after < bytes.len() && bytes[after].is_ascii_alphabetic();
        !before_is_letter && !after_is_letter
    })
}

/// Does a client call's first argument end at the literal's closing quote, or does the source
/// keep going as an expression from there?
///
/// A closing quote is not proof the argument is complete: `fetch("/api/users/" + userId)` keeps
/// going right past it, and the module's own contract (line 17) says a path built at runtime
/// must be recorded as unresolved, not approximated from the leading literal. This walks forward
/// from just past the quote, skipping whitespace and `//` / `/* */` comments — including across
/// line breaks, so a newline inserted between the quote and the `+` can't hide the continuation
/// — and inspects the next syntactically meaningful character: a comma or closing parenthesis
/// means the argument ended here; anything else (`+`, `.concat(`, …) means it didn't.
fn literal_argument_ends_here(text: &str, mut i: usize) -> bool {
    loop {
        let Some(rest) = text.get(i..) else {
            return false;
        };
        let Some(c) = rest.chars().next() else {
            return false;
        };
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if let Some(after) = rest.strip_prefix("//") {
            i += 2 + after.find('\n').map(|o| o + 1).unwrap_or(after.len());
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            i += 2 + after.find("*/").map(|o| o + 2).unwrap_or(after.len());
            continue;
        }
        return matches!(c, ',' | ')');
    }
}

/// Byte offset of the first non-whitespace character at or after `i`, or the text's length if
/// none remain. Used to find where a call's first argument actually starts once the opening
/// `(` has been located, so the character there can be inspected before deciding how to read
/// the rest of the argument.
fn skip_whitespace(text: &str, mut i: usize) -> usize {
    while let Some(c) = text.get(i..).and_then(|s| s.chars().next()) {
        if !c.is_whitespace() {
            break;
        }
        i += c.len_utf8();
    }
    i
}

/// The fallback for a client call whose first argument does not start with a quote —
/// `fetch(url)`, `fetch(baseUrl + "/api/users")`, `axios.get(endpoint)`. The literal-leading
/// patterns in [`Patterns::http`] never match these at all (there is no leading `["'`]` for
/// their capture group to anchor on), so before this fallback existed such a call produced no
/// boundary whatsoever — not even an unresolved one — silently dropping it from unmatched
/// and unresolved reporting. That is a bigger honesty gap than the one `literal_argument_ends_here`
/// closes: a build-time-unknown path is still a call to *some* endpoint, and this module's own
/// contract (line 17) says it belongs in the index as unresolved, not omitted.
///
/// Returns the argument's source text exactly as written, from `start` up to (not including)
/// the top-level comma or closing parenthesis that ends it — mirroring how a real parser would
/// find an argument boundary, but without building one. Nested `(`, `[`, `{` and their closers
/// are depth-tracked so `fetch(getUrl())` and `fetch(cfg[key])` don't truncate early, and
/// quoted substrings (`"`, `'`, `` ` ``) are skipped as opaque runs so a comma or paren inside a
/// string literal embedded in the expression doesn't end the argument prematurely.
///
/// This is a single forward scan over the remaining text with no backtracking and no
/// regex — the argument list of one call cannot make it re-examine text it has already passed,
/// so it cannot become catastrophic the way an unbounded regex alternation could.
fn first_argument_expression(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = start;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' if depth == 0 => return Some(text[start..i].trim()),
            b')' | b']' | b'}' => {
                depth -= 1;
                i += 1;
            }
            b',' if depth == 0 => return Some(text[start..i].trim()),
            quote @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Extract every boundary fragment in one file's text.
pub fn scan_boundaries(path: &Path, text: &str, language: &str) -> Vec<Boundary> {
    let p = patterns();
    let rel = path.to_string_lossy().replace('\\', "/");
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    // Line indices whose `fn` was already reported via a preceding `#[wasm_bindgen]`
    // look-ahead, so the separate `extern "C" fn` matcher below doesn't also report it —
    // `#[wasm_bindgen]\npub extern "C" fn foo() {}` is one export, not two.
    let mut wasm_attr_consumed_lines: std::collections::HashSet<usize> =
        std::collections::HashSet::new();

    for (n, line) in lines.iter().enumerate() {
        let line_no = n + 1;
        // Offset of this line within the original text, so a client call's continuation can be
        // checked past the end of the line the literal itself sits on.
        let line_offset = line.as_ptr() as usize - text.as_ptr() as usize;

        for (re, method_group, path_group, side) in &p.http {
            for caps in re.captures_iter(line) {
                let Some(raw_match) = caps.get(*path_group) else {
                    continue;
                };
                let raw = raw_match.as_str();
                if !looks_like_a_path(raw) {
                    continue;
                }
                let method = method_group
                    .and_then(|g| caps.get(g))
                    .map(|m| m.as_str().to_uppercase())
                    // `all`/`use`/`Request` are not methods — they mean "any", which is
                    // unstated, not a verb.
                    .filter(|m| m != "ALL" && m != "USE" && m != "REQUEST");

                // A client call's path can be built at runtime past the leading literal —
                // `fetch("/api/users/" + userId)` — and the closing quote is not proof the
                // argument ended there. Servers' route literals never share this problem (a
                // decorator's or router's whole argument list is just the one string), so this
                // only applies to the consuming side.
                let unresolved = *side == Side::Consumes
                    && !literal_argument_ends_here(text, line_offset + raw_match.end() + 1);

                let key = if unresolved {
                    String::new()
                } else {
                    let norm = normalise_path(raw);
                    match &method {
                        Some(m) => format!("{m} {norm}"),
                        None => norm,
                    }
                };
                out.push(Boundary {
                    kind: BoundaryKind::Http,
                    side: *side,
                    key,
                    method,
                    raw: raw.to_owned(),
                    language: language.to_owned(),
                    line: line_no,
                    path: rel.clone(),
                });
            }
        }

        // A client call whose first argument isn't a literal at all — `fetch(url)`,
        // `axios.get(endpoint)`, `requests.post(base + path)` — never matches the patterns
        // above, since those require a quote to follow the opening paren. Find the same call
        // heads again, and for any whose argument does NOT start with a quote (the quote case
        // was already handled above; re-reporting it here would double-count it), record it as
        // an unresolved consumer instead of dropping it.
        for (re, method_group) in &p.http_client_head {
            for caps in re.captures_iter(line) {
                let head_end = line_offset + caps.get(0).expect("group 0 always matches").end();
                let arg_start = skip_whitespace(text, head_end);
                match text.get(arg_start..).and_then(|s| s.chars().next()) {
                    None | Some(')') => continue, // no argument, or the call site is degenerate
                    Some('"' | '\'' | '`') => continue, // literal-leading: handled above
                    Some(_) => {}
                }
                let Some(raw) = first_argument_expression(text, arg_start) else {
                    continue;
                };
                if raw.is_empty() {
                    continue;
                }
                let method = method_group
                    .and_then(|g| caps.get(g))
                    .map(|m| m.as_str().to_uppercase());
                out.push(Boundary {
                    kind: BoundaryKind::Http,
                    side: Side::Consumes,
                    key: String::new(),
                    method,
                    raw: raw.to_owned(),
                    language: language.to_owned(),
                    line: line_no,
                    path: rel.clone(),
                });
            }
        }

        // Rust exports across the FFI boundary.
        if p.wasm_export.is_match(line) {
            // The attribute is on its own line; the item follows within a few lines.
            if let Some((offset, name)) = lines
                .iter()
                .skip(n + 1)
                .take(4)
                .enumerate()
                .find_map(|(i, l)| p.exported_fn.captures(l).map(|c| (i, c[1].to_owned())))
            {
                // Mark the line the `fn` itself sits on as already accounted for, so an
                // `extern "C" fn` on that same line isn't reported again below.
                wasm_attr_consumed_lines.insert(n + 1 + offset);
                out.push(Boundary {
                    kind: BoundaryKind::Ffi,
                    side: Side::Provides,
                    key: ffi_key(&name),
                    method: None,
                    raw: name,
                    language: language.to_owned(),
                    line: line_no,
                    path: rel.clone(),
                });
            }
        }
        if !wasm_attr_consumed_lines.contains(&n) {
            if let Some(caps) = p.ffi_export.captures(line) {
                let name = caps[1].to_owned();
                out.push(Boundary {
                    kind: BoundaryKind::Ffi,
                    side: Side::Provides,
                    key: ffi_key(&name),
                    method: None,
                    raw: name,
                    language: language.to_owned(),
                    line: line_no,
                    path: rel.clone(),
                });
            }
        }
        // JS importing wasm/FFI bindings: each named import is one consumed symbol.
        if let Some(caps) = p
            .wasm_import
            .captures(line)
            .filter(|caps| looks_like_a_wasm_or_ffi_module(&caps[2]))
        {
            for name in caps[1].split(',') {
                // `import { a as b }` — the local alias is irrelevant; the exported name is
                // what crosses the boundary.
                let name = name.split(" as ").next().unwrap_or(name).trim();
                if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                out.push(Boundary {
                    kind: BoundaryKind::Ffi,
                    side: Side::Consumes,
                    key: ffi_key(name),
                    method: None,
                    raw: name.to_owned(),
                    language: language.to_owned(),
                    line: line_no,
                    path: rel.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str, lang: &str) -> Vec<Boundary> {
        scan_boundaries(Path::new("/r/f"), text, lang)
    }

    fn keys(bs: &[Boundary], side: Side) -> Vec<String> {
        bs.iter()
            .filter(|b| b.side == side)
            .map(|b| b.key.clone())
            .collect()
    }

    // ── normalise_path ───────────────────────────────────────────────────────

    #[test]
    fn every_parameter_syntax_collapses_to_the_same_key() {
        // The whole point: five frameworks spell a path parameter five ways, and they all
        // describe the same endpoint.
        for raw in [
            "/api/models/{model_path}/animation", // FastAPI, Spring
            "/api/models/<int:id>/animation",     // Flask
            "/api/models/:id/animation",          // Express, Rails
            "/api/models/${modelPath}/animation", // JS template literal
            "/api/models/*/animation",            // wildcard
        ] {
            assert_eq!(
                normalise_path(raw),
                "/api/models/*/animation",
                "failed for {raw}"
            );
        }
    }

    #[test]
    fn scheme_host_query_and_fragment_are_stripped() {
        assert_eq!(
            normalise_path("https://api.example.com/v1/users"),
            "/v1/users"
        );
        assert_eq!(normalise_path("/v1/users?page=2"), "/v1/users");
        assert_eq!(normalise_path("/v1/users#top"), "/v1/users");
        assert_eq!(normalise_path("https://example.com"), "/");
    }

    #[test]
    fn case_is_folded_but_parameter_segments_are_not_invented() {
        assert_eq!(normalise_path("/API/Users"), "/api/users");
        assert_eq!(normalise_path("/api/users/"), "/api/users");
    }

    // ── ffi_key ──────────────────────────────────────────────────────────────

    #[test]
    fn snake_case_and_camel_case_spellings_of_one_export_share_a_key() {
        // wasm-bindgen renames snake_case to camelCase in the bindings it generates. Matching
        // literally would report every export as uncalled AND every caller as calling nothing:
        // two lists of false findings describing one working boundary.
        assert_eq!(ffi_key("auto_detect_chains"), ffi_key("autoDetectChains"));
        assert_eq!(ffi_key("solve_ik"), "ffi:solveik");
    }

    // ── the three honesty rules ──────────────────────────────────────────────

    #[test]
    fn a_captured_string_that_is_not_a_path_is_rejected() {
        // Rule 1. These lines all match a route-shaped regex but capture something that could
        // never be an endpoint. Storing them would be noise no join can ever resolve.
        let junk = r#"
            app.get(".sidebar > li")
            app.post("application/json")
            router.get("click")
        "#;
        assert!(
            scan(junk, "javascript").is_empty(),
            "CSS selectors, MIME types and event names must be rejected: {:?}",
            scan(junk, "javascript")
        );
    }

    #[test]
    fn a_runtime_concatenation_is_recorded_but_never_joinable() {
        // Rule 1's other half: a path built at runtime is unresolved, not approximated from
        // its leading literal. Before the fix, the closing quote of "/api/users/" was treated
        // as proof the argument had ended, so this returned key="/api/users" (normalised,
        // trailing slash stripped) with is_unresolved() false — a resolved endpoint invented
        // out of a call nothing said was complete.
        let bs = scan(r#"fetch("/api/users/" + userId)"#, "javascript");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].side, Side::Consumes);
        assert_eq!(bs[0].kind, BoundaryKind::Http);
        assert!(bs[0].key.is_empty(), "{bs:?}");
        assert!(bs[0].is_unresolved(), "{bs:?}");
        assert_eq!(bs[0].method, None);
        assert_eq!(bs[0].line, 1);

        // The literal-only sibling call is untouched: a call whose argument really does end at
        // the closing quote still resolves normally.
        let literal = scan(r#"fetch("/api/users/")"#, "javascript");
        assert_eq!(literal.len(), 1);
        assert!(!literal[0].is_unresolved());
        assert_eq!(literal[0].key, "/api/users");
    }

    #[test]
    fn a_stated_method_concatenation_still_reports_an_empty_key() {
        // A concatenated path with a stated verb must not become "POST " or a wildcard guess —
        // the method is real, the key is still unresolved.
        let bs = scan(r#"axios.post("/api/users/" + userId)"#, "javascript");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].method.as_deref(), Some("POST"));
        assert!(bs[0].key.is_empty(), "{bs:?}");
        assert!(bs[0].is_unresolved(), "{bs:?}");
    }

    #[test]
    fn a_comment_or_line_break_before_the_plus_does_not_hide_the_concatenation() {
        // The continuation can be pushed past the literal's own line, or past a comment, by
        // formatting alone. Neither should let a runtime-built path slip through as resolved.
        let bs = scan("fetch(\"/api/users/\"\n  + userId)", "javascript");
        assert_eq!(bs.len(), 1);
        assert!(bs[0].is_unresolved(), "{bs:?}");

        let bs = scan("fetch(\"/api/users/\" /* id */ + userId)", "javascript");
        assert_eq!(bs.len(), 1);
        assert!(bs[0].is_unresolved(), "{bs:?}");
    }

    #[test]
    fn a_nonliteral_leading_client_call_is_still_recorded_as_unresolved() {
        // Rule 1's third half: `literal_argument_ends_here` above only fires once a leading
        // literal has already been captured, so it never sees a call whose argument doesn't
        // start with a quote at all — `fetch(url)`, `fetch(baseUrl + "/api/users")`. Before
        // this fallback existed, none of the `## clients` patterns matched these lines, so
        // `scan` returned an empty vector: not an unresolved boundary, nothing. That silently
        // dropped the call from unmatched/unresolved reporting instead of flagging it.
        let cases: [(&str, &str, Option<&str>); 5] = [
            (r#"fetch(baseUrl + "/api/users")"#, "javascript", None),
            (r#"fetch(url)"#, "javascript", None),
            (r#"axios.get(endpoint)"#, "javascript", Some("GET")),
            (r#"requests.post(base + path)"#, "python", Some("POST")),
            (r#"new WebSocket(wsUrl)"#, "javascript", None),
        ];
        for (src, lang, method) in cases {
            let bs = scan(src, lang);
            assert_eq!(bs.len(), 1, "{src}: {bs:?}");
            assert_eq!(bs[0].kind, BoundaryKind::Http, "{src}: {bs:?}");
            assert_eq!(bs[0].side, Side::Consumes, "{src}: {bs:?}");
            assert!(bs[0].key.is_empty(), "{src}: {bs:?}");
            assert!(bs[0].is_unresolved(), "{src}: {bs:?}");
            assert_eq!(bs[0].method.as_deref(), method, "{src}: {bs:?}");
        }

        // Positive control: the same four call shapes, literal-leading, still resolve exactly
        // as before — the fallback only fires when the primary patterns didn't already match,
        // so it can't shadow or double-count a call the module already knew how to read.
        for (src, key, method) in [
            (r#"fetch("/api/users")"#, "/api/users", None),
            (r#"axios.get("/api/users")"#, "GET /api/users", Some("GET")),
            (
                r#"requests.post("/api/users")"#,
                "POST /api/users",
                Some("POST"),
            ),
            (r#"new WebSocket("/api/users")"#, "/api/users", None),
        ] {
            let lang = if src.starts_with("requests") {
                "python"
            } else {
                "javascript"
            };
            let bs = scan(src, lang);
            assert_eq!(bs.len(), 1, "{src}: {bs:?}");
            assert!(!bs[0].is_unresolved(), "{src}: {bs:?}");
            assert_eq!(bs[0].key, key, "{src}: {bs:?}");
            assert_eq!(bs[0].method.as_deref(), method, "{src}: {bs:?}");
        }
    }

    #[test]
    fn an_unstated_method_is_never_coerced_to_get() {
        // Rule 2. A bare `fetch` with its options object built elsewhere genuinely does not
        // say which verb it uses. Recording GET would invent a method mismatch (or hide a
        // real one) on the join.
        let bs = scan(r#"fetch("/api/users")"#, "javascript");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].method, None, "unstated is not GET");
        assert_eq!(bs[0].key, "/api/users", "no method prefix on the key");

        let stated = scan(r#"axios.post("/api/users")"#, "javascript");
        assert_eq!(stated[0].method.as_deref(), Some("POST"));
        assert_eq!(stated[0].key, "POST /api/users");
    }

    #[test]
    fn display_always_uses_the_spelling_the_source_wrote() {
        // Rule 3. The folded FFI key appears nowhere in the source; printing it would make a
        // reader hunt for a name that does not exist. A filter that cannot find its own output
        // is worse than no filter.
        let bs = scan("#[wasm_bindgen]\npub fn auto_detect_chains() {}", "rust");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].key, "ffi:autodetectchains", "folded for matching");
        assert_eq!(bs[0].raw, "auto_detect_chains", "kept for display");
        assert_eq!(bs[0].label(), "ffi:auto_detect_chains");

        let http = scan(r#"@app.post("/api/models/{id}/run")"#, "python");
        assert_eq!(http[0].key, "POST /api/models/*/run");
        assert_eq!(
            http[0].label(),
            "POST /api/models/{id}/run",
            "raw, not folded"
        );
    }

    // ── the receiver-name distinction ────────────────────────────────────────

    #[test]
    fn app_get_declares_and_axios_get_calls_from_otherwise_identical_lines() {
        // The single most load-bearing discrimination in the whole module: these two lines are
        // character-for-character identical apart from the receiver.
        let server = scan(r#"app.get("/api/users", handler)"#, "javascript");
        let client = scan(r#"axios.get("/api/users")"#, "javascript");
        assert_eq!(server.len(), 1);
        assert_eq!(client.len(), 1);
        assert_eq!(server[0].side, Side::Provides);
        assert_eq!(client[0].side, Side::Consumes);
        assert_eq!(server[0].key, client[0].key, "and they join");
    }

    // ── per-language providers ───────────────────────────────────────────────

    #[test]
    fn python_decorators_are_recognised() {
        let bs = scan(
            "@app.get(\"/health\")\n@router.post(\"/api/items\")\n@app.route(\"/legacy\")",
            "python",
        );
        let k = keys(&bs, Side::Provides);
        assert!(k.contains(&"GET /health".to_owned()), "{k:?}");
        assert!(k.contains(&"POST /api/items".to_owned()), "{k:?}");
        // `@app.route` states no verb.
        assert!(k.contains(&"/legacy".to_owned()), "{k:?}");
    }

    #[test]
    fn rust_axum_and_attribute_routes_are_recognised() {
        let bs = scan(
            ".route(\"/api/stats\", get(api_stats))\n#[post(\"/api/submit\")]",
            "rust",
        );
        let k = keys(&bs, Side::Provides);
        assert!(k.contains(&"GET /api/stats".to_owned()), "{k:?}");
        assert!(k.contains(&"POST /api/submit".to_owned()), "{k:?}");
    }

    #[test]
    fn go_spring_and_aspnet_providers_are_recognised() {
        let go = scan(r#"mux.HandleFunc("/api/go", handler)"#, "go");
        assert_eq!(
            keys(&go, Side::Provides),
            vec!["/api/go"],
            "Go states no verb"
        );

        let spring = scan(r#"@PostMapping("/api/java")"#, "java");
        assert_eq!(keys(&spring, Side::Provides), vec!["POST /api/java"]);

        let net = scan(r#"[HttpGet("/api/cs")]"#, "csharp");
        assert_eq!(keys(&net, Side::Provides), vec!["GET /api/cs"]);
    }

    // ── clients ──────────────────────────────────────────────────────────────

    #[test]
    fn every_client_shape_is_recognised() {
        let bs = scan(
            "fetch(\"/a\")\naxios.get(\"/b\")\nrequests.post(\"/c\")\nnew WebSocket(\"/d\")",
            "javascript",
        );
        let k = keys(&bs, Side::Consumes);
        assert!(k.contains(&"/a".to_owned()), "{k:?}");
        assert!(k.contains(&"GET /b".to_owned()), "{k:?}");
        assert!(k.contains(&"POST /c".to_owned()), "{k:?}");
        assert!(k.contains(&"/d".to_owned()), "{k:?}");
    }

    // ── FFI ──────────────────────────────────────────────────────────────────

    #[test]
    fn wasm_bindgen_looks_ahead_past_intervening_lines() {
        // The attribute sits on its own line; the item may follow after doc comments.
        let bs = scan(
            "#[wasm_bindgen]\n/// docs\n#[inline]\npub fn solve_ik() {}",
            "rust",
        );
        assert_eq!(bs.len(), 1, "{bs:?}");
        assert_eq!(bs[0].raw, "solve_ik");
    }

    #[test]
    fn extern_c_exports_are_recognised() {
        let bs = scan(r#"pub extern "C" fn indexa_init() {}"#, "rust");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].side, Side::Provides);
        assert_eq!(bs[0].key, "ffi:indexainit");
    }

    #[test]
    fn a_wasm_export_with_an_explicit_extern_c_signature_is_recorded_once() {
        // Regression: the attribute look-ahead found `foo` via `fn foo`, and the separate
        // `extern "C" fn` matcher found the very same `foo` again on the line the look-ahead
        // landed on — one export, reported twice.
        let bs = scan("#[wasm_bindgen]\npub extern \"C\" fn foo() {}", "rust");
        assert_eq!(bs.len(), 1, "{bs:?}");
        assert_eq!(bs[0].raw, "foo");

        // An `extern "C"` export with no preceding `#[wasm_bindgen]` is untouched by the
        // dedup and still reported normally.
        assert_eq!(
            scan(r#"pub extern "C" fn indexa_init() {}"#, "rust").len(),
            1
        );
    }

    #[test]
    fn a_wasm_import_records_one_consumer_per_named_symbol() {
        let bs = scan(
            r#"import { solve_ik, auto_detect_chains } from './avatar_ik_wasm.js'"#,
            "javascript",
        );
        assert_eq!(bs.len(), 2, "{bs:?}");
        assert!(bs.iter().all(|b| b.side == Side::Consumes));
        let k = keys(&bs, Side::Consumes);
        assert!(k.contains(&"ffi:solveik".to_owned()), "{k:?}");
        assert!(k.contains(&"ffi:autodetectchains".to_owned()), "{k:?}");
    }

    #[test]
    fn an_aliased_wasm_import_keys_on_the_exported_name_not_the_alias() {
        let bs = scan(
            r#"import { solve_ik as solve } from './wasm_bg.js'"#,
            "javascript",
        );
        assert_eq!(bs.len(), 1, "{bs:?}");
        assert_eq!(
            bs[0].raw, "solve_ik",
            "the exported name crosses, not the alias"
        );
    }

    #[test]
    fn an_ordinary_js_import_is_not_an_ffi_boundary() {
        // Only modules whose specifier looks like a wasm/FFI binding count. Treating every
        // named import as a boundary would drown the real ones.
        assert!(scan(r#"import { useState } from 'react'"#, "javascript").is_empty());
    }

    #[test]
    fn a_filename_that_merely_contains_the_substring_ffi_is_not_an_ffi_boundary() {
        // Regression: "office" and "traffic" both contain the literal substring "ffi",
        // and a bare substring check treated importing from either file as an FFI boundary.
        assert!(
            scan(r#"import { useState } from './office.js'"#, "javascript").is_empty(),
            "office.js is not an FFI module"
        );
        assert!(
            scan(r#"import { report } from './traffic.js'"#, "javascript").is_empty(),
            "traffic.js is not an FFI module"
        );

        // A genuine ffi-flavored specifier still matches, whether "ffi" is its own segment
        // or attached with an underscore/hyphen on either side.
        for spec in [
            "./ffi.js",
            "./my_ffi.js",
            "./ffi_bridge.js",
            "./my-ffi-lib.js",
        ] {
            let bs = scan(&format!("import {{ solve }} from '{spec}'"), "javascript");
            assert_eq!(bs.len(), 1, "{spec}: {bs:?}");
        }
    }

    // ── the end-to-end join shape ────────────────────────────────────────────

    #[test]
    fn a_python_route_and_a_javascript_fetch_produce_the_same_join_key() {
        // The motivating example, both halves, from two files in two languages.
        let server = scan(
            r#"@app.post("/api/models/{model_path:path}/animation")"#,
            "python",
        );
        let client = scan(
            r#"fetch(`/api/models/${modelPath}/animation`, { method: 'POST' })"#,
            "javascript",
        );
        assert_eq!(server.len(), 1);
        assert_eq!(client.len(), 1);
        assert_eq!(server[0].side, Side::Provides);
        assert_eq!(client[0].side, Side::Consumes);
        // The server states POST; the client's verb lives in an options object the regex
        // deliberately does not read, so it is unstated. The PATHS agree, which is what a
        // method-mismatch report is built from.
        assert_eq!(server[0].key, "POST /api/models/*/animation");
        assert_eq!(client[0].key, "/api/models/*/animation");
        assert_eq!(client[0].method, None);
    }

    #[test]
    fn a_python_decorator_is_not_also_read_as_an_express_route() {
        // Regression: `\b(?:app|…)` matched the `app.get` inside Python's `@app.get("/x")`,
        // so every FastAPI decorator was captured twice — once correctly, once as an Express
        // route in the wrong language. Caught by an exact-count assertion, which is why the
        // per-language tests assert counts rather than just membership.
        let bs = scan(r#"@app.get("/health")"#, "python");
        assert_eq!(bs.len(), 1, "captured twice: {bs:?}");
        assert_eq!(bs[0].language, "python");

        // And a receiver that merely ENDS in `app` is not `app`.
        assert!(
            scan(r#"myapp.get("/x")"#, "javascript").is_empty(),
            "`myapp.get` is not the Express `app.get`"
        );

        // The real Express form still works, at line start and mid-line.
        assert_eq!(scan(r#"app.get("/x", h)"#, "javascript").len(), 1);
        assert_eq!(scan(r#"  router.post("/y", h)"#, "javascript").len(), 1);
    }

    #[test]
    fn line_numbers_and_paths_are_recorded_for_every_fragment() {
        let bs = scan_boundaries(
            Path::new("/r/server.py"),
            "import x\n\n@app.get(\"/health\")\ndef health(): ...",
            "python",
        );
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].line, 3, "1-based, so a reader can jump to it");
        assert_eq!(bs[0].path, "/r/server.py");
        assert_eq!(bs[0].language, "python");
    }

    #[test]
    fn a_file_with_no_boundaries_produces_nothing() {
        assert!(scan("fn main() { println!(\"hello\"); }", "rust").is_empty());
        assert!(scan("", "rust").is_empty());
    }

    #[test]
    fn multiple_fragments_on_one_line_are_all_captured() {
        let bs = scan(r#"fetch("/a"); fetch("/b");"#, "javascript");
        assert_eq!(bs.len(), 2, "{bs:?}");
    }
}
