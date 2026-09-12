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
            // `#[wasm_bindgen]` sits on its own line above the item, so the exported name is
            // found by looking ahead rather than on this line.
            wasm_export: r(r#"#\[wasm_bindgen"#),
            ffi_export: r(r#"extern\s+"C"\s+fn\s+(\w+)"#),
            exported_fn: r(r#"\bfn\s+(\w+)"#),
            // `import { solve_ik, init } from './avatar_ik_wasm.js'`
            wasm_import: r(r#"import\s*\{([^}]+)\}\s*from\s*["'`]([^"'`]*(?:wasm|_bg|ffi)[^"'`]*)["'`]"#),
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

/// Extract every boundary fragment in one file's text.
pub fn scan_boundaries(path: &Path, text: &str, language: &str) -> Vec<Boundary> {
    let p = patterns();
    let rel = path.to_string_lossy().replace('\\', "/");
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

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

        // Rust exports across the FFI boundary.
        if p.wasm_export.is_match(line) {
            // The attribute is on its own line; the item follows within a few lines.
            if let Some(name) = lines
                .iter()
                .skip(n + 1)
                .take(4)
                .find_map(|l| p.exported_fn.captures(l).map(|c| c[1].to_owned()))
            {
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
        // JS importing wasm/FFI bindings: each named import is one consumed symbol.
        if let Some(caps) = p.wasm_import.captures(line) {
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
