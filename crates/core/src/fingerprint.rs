//! Software / project-type fingerprinting by file-pattern signatures.
//!
//! A [`FingerprintDef`] matches a directory when its marker expressions are satisfied by that
//! directory's contents. Markers come in three forms, chosen by the string's shape:
//!
//! - **direct child** — `Cargo.toml` — a file/dir named exactly that, directly under the dir.
//! - **child glob** — `*.xcodeproj` — a direct child whose *name* matches the `*`/`?` pattern.
//! - **relative path** — `Contents/Info.plist` — a nested entry at that path under the dir
//!   (so a macOS `.app` bundle, an Xcode project, a `.github/workflows` dir, … are expressible).
//!
//! A def can require `all_of` (every marker), `any_of` (at least one), and forbid `none_of`
//! (anti-markers). `specificity` ranks overlapping matches so the most specific wins
//! (Next.js over a bare Node package); `family` buckets the def into one of four families
//! (`code`/`os`/`infra`/`data`). Definitions come from an embedded default library, an optional
//! seeded library (generated offline from external sources — see `tools/`), and a user JSON file,
//! so the catalog is extensible without recompiling.
//!
//! Detection runs over the already-indexed entry paths (no extra filesystem walk), so it is
//! instant once a `scan` has been done. [`detect`] is pure (no I/O) for testability.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Default specificity for a def that doesn't set one — the base tier. More specific defs
/// (e.g. a framework that also implies a language) should set a higher value so they win.
fn default_specificity() -> u32 {
    10
}

/// Provenance for a seeded rule — which external source it came from, its license, and the
/// version/commit pulled. Hand-authored rules leave this `None`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    pub source: String,
    pub license: String,
    pub version: String,
}

/// One fingerprint pattern. Backward-compatible: a def with only `name`/`category`/`all_of`
/// (the pre-v0.66 shape) still deserializes — every field added since is `#[serde(default)]`.
#[derive(Debug, Clone, Deserialize)]
pub struct FingerprintDef {
    /// Human-readable name, e.g. "Rust crate" or "Next.js app".
    pub name: String,
    /// Coarse legacy category, e.g. "code" or "infra". Kept for the old format; `family`
    /// supersedes it for the four-family taxonomy (falls back to this when unset).
    pub category: String,
    /// One-line description shown by `indexa fingerprint`.
    #[serde(default)]
    pub description: String,
    /// Markers that must ALL be satisfied for a match (the common case).
    #[serde(default)]
    pub all_of: Vec<String>,
    /// Markers of which at least ONE must be satisfied (empty ⇒ no `any_of` constraint).
    #[serde(default)]
    pub any_of: Vec<String>,
    /// Anti-markers: if ANY of these is satisfied the def does NOT match. Use sparingly —
    /// prefer `specificity` for "X over Y"; reserve this for genuine false-positive suppression
    /// (e.g. a Terraform module should not fire inside a `.terraform/` cache dir).
    #[serde(default)]
    pub none_of: Vec<String>,
    /// Stable machine id, e.g. "nextjs_app", "macos_app_bundle". Defaults to a slug of `name`.
    #[serde(default)]
    pub kind: Option<String>,
    /// One of `code`/`os`/`infra`/`data`. Defaults to `category`.
    #[serde(default)]
    pub family: Option<String>,
    /// Ranking for most-specific-wins. Higher beats lower at the same directory.
    #[serde(default = "default_specificity")]
    pub specificity: u32,
    /// Where a seeded rule came from (hand-authored rules: `None`).
    #[serde(default)]
    pub provenance: Option<Provenance>,
}

impl FingerprintDef {
    /// The stable machine id: explicit `kind`, else a slug of `name`.
    pub fn kind_id(&self) -> String {
        self.kind.clone().unwrap_or_else(|| slugify(&self.name))
    }

    /// The taxonomy family: explicit `family`, else the legacy `category`.
    pub fn family_id(&self) -> String {
        self.family.clone().unwrap_or_else(|| self.category.clone())
    }

    /// True when the def can match anything — it needs at least one positive marker, so a
    /// `none_of`-only def never matches every directory.
    fn has_positive_marker(&self) -> bool {
        !self.all_of.is_empty() || !self.any_of.is_empty()
    }
}

/// A detected instance of a fingerprint: the directories where it matched, plus the taxonomy
/// fields needed to rank and persist it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub name: String,
    pub category: String,
    pub description: String,
    pub kind: String,
    pub family: String,
    pub specificity: u32,
    /// The positive marker strings (`all_of` ∪ `any_of`) that defined this def — recorded for
    /// the per-dir `markers_json` ("why was this detected?").
    pub markers: Vec<String>,
    pub paths: Vec<String>,
}

/// A single parsed marker expression.
#[derive(Debug, Clone)]
enum Marker {
    /// A file/dir named exactly this, directly under the candidate dir.
    DirectChild(String),
    /// A direct child whose name matches this `*`/`?` glob.
    ChildGlob(String),
    /// A nested entry at this (literal) relative path under the candidate dir.
    RelPath(String),
}

/// Parse a marker string into a [`Marker`]. A `**` recursive glob is unsupported — it would make
/// the evaluator super-linear — and is treated as a (non-matching) literal; the curated library
/// never uses it. A trailing `/` (a "must be a dir" hint) is trimmed; v1 only tests existence.
fn parse_marker(s: &str) -> Marker {
    let s = s.trim_end_matches('/');
    if s.contains('/') {
        // A relative path. (A glob *inside* a path is not supported in v1 — the library uses a
        // standalone child glob like `*.xcodeproj` instead — so this is treated as literal.)
        Marker::RelPath(s.to_string())
    } else if s.contains('*') || s.contains('?') {
        Marker::ChildGlob(s.to_string())
    } else {
        Marker::DirectChild(s.to_string())
    }
}

/// Full-string `*` (any run, incl. empty) / `?` (any single char) glob match. Deliberately tiny
/// and dependency-free — the markers only ever need single-`*` patterns like `*.xcodeproj`, and
/// keeping the matcher here means one identical semantics across the CLI and the seed generator.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = name.chars().collect();
    // Classic two-pointer wildcard match with backtracking on the last `*`.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);
    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// Does `marker` hold for directory `dir`, given its direct-child names and the full entry set?
fn marker_matches(
    marker: &Marker,
    dir: &str,
    children: &HashSet<String>,
    all_paths: &HashSet<String>,
) -> bool {
    match marker {
        Marker::DirectChild(name) => children.contains(name),
        Marker::ChildGlob(pat) => children.iter().any(|c| glob_match(pat, c)),
        Marker::RelPath(rel) => all_paths.contains(&format!("{dir}/{rel}")),
    }
}

/// The built-in pattern library is a separate JSON data file so it can grow without churning Rust.
pub const DEFAULT_FINGERPRINTS_JSON: &str = include_str!("fingerprints_builtin.json");

/// Seeded patterns generated OFFLINE from external sources (see `tools/`). Committed as a vendored
/// snapshot — the runtime never fetches anything. Empty `[]` until the generator is first run.
pub const SEEDED_FINGERPRINTS_JSON: &str = include_str!("fingerprints_seed.json");

/// Load fingerprint definitions: embedded defaults + seeded snapshot + any from `user_json_path`
/// (appended, so users can extend the catalog). A missing user file is not an error.
pub fn load(user_json_path: Option<&Path>) -> Result<Vec<FingerprintDef>> {
    let mut defs: Vec<FingerprintDef> = serde_json::from_str(DEFAULT_FINGERPRINTS_JSON)
        .context("parsing built-in fingerprint library")?;
    let seeded: Vec<FingerprintDef> = serde_json::from_str(SEEDED_FINGERPRINTS_JSON)
        .context("parsing seeded fingerprint library")?;
    defs.extend(seeded);
    if let Some(p) = user_json_path {
        if p.exists() {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("reading fingerprints file {}", p.display()))?;
            let user: Vec<FingerprintDef> = serde_json::from_str(&text)
                .with_context(|| format!("parsing fingerprints file {}", p.display()))?;
            defs.extend(user);
        }
    }
    Ok(defs)
}

/// Detect fingerprints over a set of indexed entry paths. Pure (no I/O) for testability; returns
/// one [`Detection`] per matching def, each carrying the directories it matched, sorted by match
/// count (descending) then name. Per-directory "winner" selection (most-specific-wins) is left to
/// the caller (the detector pass), so this stays unopinionated and `indexa fingerprint` can show
/// every match.
pub fn detect<I>(entry_paths: I, defs: &[FingerprintDef]) -> Vec<Detection>
where
    I: IntoIterator<Item = String>,
{
    let all_paths: HashSet<String> = entry_paths.into_iter().collect();

    // dir -> set of its direct-child basenames (files and subdirs alike).
    let mut children: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for path in &all_paths {
        let p = Path::new(path);
        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            children
                .entry(parent.to_string_lossy().into_owned())
                .or_default()
                .insert(name.to_string_lossy().into_owned());
        }
    }

    let mut detections: Vec<Detection> = defs
        .iter()
        .filter(|def| def.has_positive_marker())
        .filter_map(|def| {
            let all_m: Vec<Marker> = def.all_of.iter().map(|s| parse_marker(s)).collect();
            let any_m: Vec<Marker> = def.any_of.iter().map(|s| parse_marker(s)).collect();
            let none_m: Vec<Marker> = def.none_of.iter().map(|s| parse_marker(s)).collect();

            let mut paths: Vec<String> = children
                .iter()
                .filter(|(dir, kids)| {
                    all_m
                        .iter()
                        .all(|m| marker_matches(m, dir, kids, &all_paths))
                        && (any_m.is_empty()
                            || any_m
                                .iter()
                                .any(|m| marker_matches(m, dir, kids, &all_paths)))
                        && none_m
                            .iter()
                            .all(|m| !marker_matches(m, dir, kids, &all_paths))
                })
                .map(|(dir, _)| dir.clone())
                .collect();
            if paths.is_empty() {
                return None;
            }
            paths.sort();
            let markers: Vec<String> = def
                .all_of
                .iter()
                .chain(def.any_of.iter())
                .cloned()
                .collect();
            Some(Detection {
                name: def.name.clone(),
                category: def.category.clone(),
                description: def.description.clone(),
                kind: def.kind_id(),
                family: def.family_id(),
                specificity: def.specificity,
                markers,
                paths,
            })
        })
        .collect();

    detections.sort_by(|a, b| b.paths.len().cmp(&a.paths.len()).then(a.name.cmp(&b.name)));
    detections
}

/// Lowercase, non-alphanumeric → `_`, collapse runs. Used to derive a stable `kind` from a name.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_us = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, all_of: &[&str]) -> FingerprintDef {
        FingerprintDef {
            name: name.to_owned(),
            category: "code".to_owned(),
            description: String::new(),
            all_of: all_of.iter().map(|s| s.to_string()).collect(),
            any_of: vec![],
            none_of: vec![],
            kind: None,
            family: None,
            specificity: default_specificity(),
            provenance: None,
        }
    }

    #[test]
    fn default_library_parses() {
        let defs = load(None).unwrap();
        assert!(defs.iter().any(|d| d.name == "Rust crate"));
        // Every def needs at least one positive marker (else it would match every dir).
        assert!(defs.iter().all(|d| d.has_positive_marker()));
    }

    #[test]
    fn seeded_library_merges_with_provenance() {
        let defs = load(None).unwrap();
        // A long-tail manifest from the seeded (cdxgen-derived) snapshot is present...
        let swift = defs.iter().find(|d| d.kind_id() == "swift_package");
        assert!(swift.is_some(), "seeded swift_package should be loaded");
        // ...and carries provenance, unlike hand-authored built-ins.
        assert!(swift.unwrap().provenance.is_some());
        assert!(defs
            .iter()
            .find(|d| d.kind_id() == "rust_crate")
            .unwrap()
            .provenance
            .is_none());
        // No two defs share a kind (would risk a (path, app_kind) PK collision when persisting).
        let mut kinds: Vec<String> = defs.iter().map(|d| d.kind_id()).collect();
        kinds.sort();
        let before = kinds.len();
        kinds.dedup();
        assert_eq!(before, kinds.len(), "fingerprint kinds must be unique");
    }

    #[test]
    fn old_format_def_still_parses() {
        // The pre-v0.66 shape (no any_of/none_of/kind/family/specificity/provenance).
        let json = r#"[{"name":"Rust crate","category":"code","all_of":["Cargo.toml"]}]"#;
        let defs: Vec<FingerprintDef> = serde_json::from_str(json).unwrap();
        assert_eq!(defs[0].specificity, 10); // serde default applied
        assert_eq!(defs[0].kind_id(), "rust_crate"); // slug fallback
        assert_eq!(defs[0].family_id(), "code"); // category fallback
    }

    #[test]
    fn glob_matcher_handles_star_and_question() {
        assert!(glob_match("*.xcodeproj", "MyApp.xcodeproj"));
        assert!(glob_match("*.tf", "main.tf"));
        assert!(!glob_match("*.tf", "main.tfvars")); // suffix must be exact
        assert!(glob_match("file.?s", "file.js"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b", "axxc"));
        assert!(glob_match("Cargo.toml", "Cargo.toml")); // no wildcard = literal
    }

    #[test]
    fn detect_matches_all_of_signatures() {
        let defs = vec![
            def("Rust crate", &["Cargo.toml"]),
            def("Node.js / npm package", &["package.json"]),
            {
                let mut d = def("Next.js app", &["package.json", "next.config.js"]);
                d.specificity = 30;
                d
            },
        ];
        let entries = vec![
            "/proj/rustapp/Cargo.toml".to_owned(),
            "/proj/rustapp/src".to_owned(),
            "/proj/web/package.json".to_owned(),
            "/proj/web/next.config.js".to_owned(),
            "/proj/plain/README.md".to_owned(),
        ];

        let detections = detect(entries, &defs);
        let rust = detections.iter().find(|d| d.name == "Rust crate").unwrap();
        assert_eq!(rust.paths, vec!["/proj/rustapp".to_owned()]);
        // Next.js requires BOTH (all_of); Node matches package.json alone — both are reported.
        let next = detections.iter().find(|d| d.name == "Next.js app").unwrap();
        assert_eq!(next.paths, vec!["/proj/web".to_owned()]);
        assert_eq!(next.specificity, 30);
        assert!(detections
            .iter()
            .any(|d| d.name == "Node.js / npm package" && d.paths == vec!["/proj/web".to_owned()]));
        assert!(detections
            .iter()
            .all(|d| !d.paths.contains(&"/proj/plain".to_owned())));
    }

    #[test]
    fn detect_resolves_nested_relpath_markers() {
        // A macOS .app bundle: Contents/Info.plist nested under the .app dir.
        let mut app = def("macOS app bundle", &["Contents/Info.plist"]);
        app.kind = Some("macos_app_bundle".to_owned());
        app.family = Some("os".to_owned());
        let entries = vec![
            "/Applications/Indexa.app/Contents".to_owned(),
            "/Applications/Indexa.app/Contents/Info.plist".to_owned(),
            "/Applications/Indexa.app/Contents/MacOS".to_owned(),
        ];
        let d = detect(entries, std::slice::from_ref(&app));
        let hit = d.iter().find(|d| d.kind == "macos_app_bundle").unwrap();
        assert_eq!(hit.paths, vec!["/Applications/Indexa.app".to_owned()]);
        assert_eq!(hit.family, "os");
    }

    #[test]
    fn detect_resolves_child_glob_markers() {
        let xcode = {
            let mut d = def("Xcode project", &["*.xcodeproj"]);
            d.kind = Some("xcode_project".to_owned());
            d
        };
        let entries = vec![
            "/code/MyApp/MyApp.xcodeproj".to_owned(),
            "/code/MyApp/README.md".to_owned(),
        ];
        let d = detect(entries, std::slice::from_ref(&xcode));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].paths, vec!["/code/MyApp".to_owned()]);
    }

    #[test]
    fn detect_honors_any_of_and_none_of() {
        // any_of: matches if EITHER manifest is present.
        let mut py = def("Python project", &[]);
        py.any_of = vec!["pyproject.toml".to_owned(), "requirements.txt".to_owned()];
        // none_of: Terraform module, but NOT inside a .terraform cache dir.
        let mut tf = def("Terraform module", &["*.tf"]);
        tf.none_of = vec![".terraform".to_owned()];

        let entries = vec![
            "/a/requirements.txt".to_owned(),
            "/infra/main.tf".to_owned(),
            "/cache/main.tf".to_owned(),
            "/cache/.terraform".to_owned(),
        ];
        let d = detect(entries, &[py, tf]);
        assert!(d
            .iter()
            .any(|d| d.name == "Python project" && d.paths == vec!["/a".to_owned()]));
        let tfd = d.iter().find(|d| d.name == "Terraform module").unwrap();
        // /infra matches; /cache is suppressed by the .terraform anti-marker.
        assert_eq!(tfd.paths, vec!["/infra".to_owned()]);
    }

    #[test]
    fn detect_requires_every_signature() {
        let defs = vec![def("Rails app", &["Gemfile", "config.ru"])];
        // Only Gemfile present → no match (config.ru missing).
        assert!(detect(vec!["/app/Gemfile".to_owned()], &defs).is_empty());
        assert_eq!(
            detect(
                vec!["/app/Gemfile".to_owned(), "/app/config.ru".to_owned()],
                &defs
            )
            .len(),
            1
        );
    }

    #[test]
    fn none_of_only_def_matches_nothing() {
        // A def with no positive marker must never match every dir.
        let mut d = def("bad", &[]);
        d.none_of = vec!["foo".to_owned()];
        assert!(detect(vec!["/x/bar".to_owned()], &[d]).is_empty());
    }

    #[test]
    fn slugify_makes_stable_kinds() {
        assert_eq!(slugify("Next.js app"), "next_js_app");
        assert_eq!(slugify("Ruby / Bundler project"), "ruby_bundler_project");
        assert_eq!(slugify("Rust crate"), "rust_crate");
    }
}
