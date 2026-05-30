//! Software / project-type fingerprinting by file-pattern signatures.
//!
//! A [`FingerprintDef`] matches a directory when **all** of its `all_of` signature names are
//! present as direct children (files or subdirectories). Definitions come from an embedded
//! default library plus an optional user JSON file, so the catalog is community-extensible
//! without recompiling — point a contributor at the JSON format and they can add a pattern.
//!
//! Detection runs over the already-indexed entry paths (no extra filesystem walk), so
//! `indexa fingerprint` is instant once a `scan` has been done.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// One fingerprint pattern. A directory matches when every name in `all_of` is a direct child.
#[derive(Debug, Clone, Deserialize)]
pub struct FingerprintDef {
    /// Human-readable name, e.g. "Rust crate" or "Next.js app".
    pub name: String,
    /// Coarse category, e.g. "code" or "infra".
    pub category: String,
    /// One-line description shown by `indexa fingerprint`.
    #[serde(default)]
    pub description: String,
    /// Direct-child names (files or dirs) that must ALL be present for a match.
    pub all_of: Vec<String>,
}

/// A detected instance of a fingerprint: the directories where it matched.
#[derive(Debug, Clone)]
pub struct Detection {
    pub name: String,
    pub category: String,
    pub description: String,
    pub paths: Vec<String>,
}

/// The built-in pattern library (JSON). Extend it with a user file — see [`load`] and
/// `docs/fingerprints.md`. Patterns use fixed direct-child filenames to stay false-positive-free.
pub const DEFAULT_FINGERPRINTS_JSON: &str = r#"[
  {"name":"Rust crate","category":"code","description":"Cargo package","all_of":["Cargo.toml"]},
  {"name":"Node.js / npm package","category":"code","description":"package.json present","all_of":["package.json"]},
  {"name":"Next.js app","category":"code","description":"Next.js project","all_of":["package.json","next.config.js"]},
  {"name":"Python project (pyproject)","category":"code","description":"PEP 518 project","all_of":["pyproject.toml"]},
  {"name":"Python project (requirements)","category":"code","description":"pip requirements","all_of":["requirements.txt"]},
  {"name":"Go module","category":"code","description":"Go module","all_of":["go.mod"]},
  {"name":"Ruby / Bundler project","category":"code","description":"Bundler project","all_of":["Gemfile"]},
  {"name":"Java / Maven project","category":"code","description":"Maven project","all_of":["pom.xml"]},
  {"name":"Gradle project","category":"code","description":"Gradle build","all_of":["build.gradle"]},
  {"name":"Docker Compose stack","category":"infra","description":"Compose services","all_of":["docker-compose.yml"]},
  {"name":"Dockerized service","category":"infra","description":"Has a Dockerfile","all_of":["Dockerfile"]},
  {"name":"Helm chart","category":"infra","description":"Helm chart","all_of":["Chart.yaml"]},
  {"name":"Terraform module","category":"infra","description":"Terraform configuration","all_of":["main.tf"]}
]"#;

/// Load fingerprint definitions: the embedded defaults plus any from `user_json_path`
/// (appended, so users can extend the catalog). A missing user file is not an error.
pub fn load(user_json_path: Option<&Path>) -> Result<Vec<FingerprintDef>> {
    let mut defs: Vec<FingerprintDef> = serde_json::from_str(DEFAULT_FINGERPRINTS_JSON)
        .context("parsing built-in fingerprint library")?;
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

/// Detect fingerprints over a set of indexed entry paths. A directory matches a definition
/// when all of its `all_of` signature names are direct children. Pure (no I/O) for testability;
/// returns detections sorted by match count (descending), then name.
pub fn detect<I>(entry_paths: I, defs: &[FingerprintDef]) -> Vec<Detection>
where
    I: IntoIterator<Item = String>,
{
    // dir -> set of its direct-child basenames (files and subdirs alike).
    let mut children: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for path in entry_paths {
        let p = Path::new(&path);
        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            children
                .entry(parent.to_string_lossy().into_owned())
                .or_default()
                .insert(name.to_string_lossy().into_owned());
        }
    }

    let mut detections: Vec<Detection> = defs
        .iter()
        .filter_map(|def| {
            let mut paths: Vec<String> = children
                .iter()
                .filter(|(_, kids)| def.all_of.iter().all(|sig| kids.contains(sig)))
                .map(|(dir, _)| dir.clone())
                .collect();
            if paths.is_empty() {
                return None;
            }
            paths.sort();
            Some(Detection {
                name: def.name.clone(),
                category: def.category.clone(),
                description: def.description.clone(),
                paths,
            })
        })
        .collect();

    detections.sort_by(|a, b| b.paths.len().cmp(&a.paths.len()).then(a.name.cmp(&b.name)));
    detections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_library_parses() {
        let defs = load(None).unwrap();
        assert!(defs.iter().any(|d| d.name == "Rust crate"));
        assert!(defs.iter().all(|d| !d.all_of.is_empty()));
    }

    #[test]
    fn detect_matches_all_of_signatures() {
        let defs = load(None).unwrap();
        let entries = vec![
            "/proj/rustapp/Cargo.toml".to_owned(),
            "/proj/rustapp/src".to_owned(),
            "/proj/web/package.json".to_owned(),
            "/proj/web/next.config.js".to_owned(),
            "/proj/plain/README.md".to_owned(),
        ]
        .into_iter();

        let detections = detect(entries, &defs);
        let rust = detections.iter().find(|d| d.name == "Rust crate").unwrap();
        assert_eq!(rust.paths, vec!["/proj/rustapp".to_owned()]);

        // Next.js requires BOTH package.json AND next.config.js (all_of).
        let next = detections.iter().find(|d| d.name == "Next.js app").unwrap();
        assert_eq!(next.paths, vec!["/proj/web".to_owned()]);
        // /proj/web is also a Node package (package.json alone).
        assert!(detections
            .iter()
            .any(|d| d.name == "Node.js / npm package" && d.paths == vec!["/proj/web".to_owned()]));
        // No false positive for the plain dir.
        assert!(detections
            .iter()
            .all(|d| !d.paths.contains(&"/proj/plain".to_owned())));
    }

    #[test]
    fn detect_requires_every_signature() {
        let defs = vec![FingerprintDef {
            name: "Rails app".to_owned(),
            category: "code".to_owned(),
            description: String::new(),
            all_of: vec!["Gemfile".to_owned(), "config.ru".to_owned()],
        }];
        // Only Gemfile present → no match (config.ru missing).
        let entries = vec!["/app/Gemfile".to_owned()].into_iter();
        assert!(detect(entries, &defs).is_empty());

        let entries = vec!["/app/Gemfile".to_owned(), "/app/config.ru".to_owned()].into_iter();
        assert_eq!(detect(entries, &defs).len(), 1);
    }
}
