//! Post-index pass that recognizes which application/stack/structure each directory is and
//! persists it to `directory_apps` (v0.66).
//!
//! This is a **sibling** of the decision detectors (`decisions::detectors::run_detectors`), NOT
//! part of it: `run_detectors` opens human-judgment questions under a `ReviewConfig` with fatigue
//! caps, whereas app detection writes deterministic, re-derivable facts and asks nothing. It runs
//! over the already-indexed entry paths (no filesystem walk) via [`fingerprint::detect`], picks the
//! most-specific match per directory as the primary, and rewrites the table so a re-index that
//! changed (or removed) a folder's shape self-corrects. Fail-open at the call site — a detection
//! error must never fail a completed index.

use crate::fingerprint::{self, FingerprintDef};
use crate::store::{DetectedApp, Store};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// Detect directory applications over the whole index and persist them. Returns the number of
/// `directory_apps` rows written. `defs` is the loaded fingerprint library (caller resolves the
/// optional user `fingerprints.json` path, so core stays config-agnostic).
pub fn detect_directory_apps(store: &mut Store, defs: &[FingerprintDef]) -> Result<usize> {
    let detections = fingerprint::detect(store.all_entry_paths()?, defs);

    // Invert def-major detections into dir -> [matching defs].
    let mut by_dir: HashMap<String, Vec<&fingerprint::Detection>> = HashMap::new();
    for det in &detections {
        for path in &det.paths {
            by_dir.entry(path.clone()).or_default().push(det);
        }
    }

    // Dirs that previously had rows — any not matched this pass must be cleared (re-derivable).
    let existing: HashSet<String> = store
        .all_detected_apps()?
        .into_iter()
        .map(|a| a.path)
        .collect();
    let matched: HashSet<String> = by_dir.keys().cloned().collect();

    let mut written = 0usize;
    for (dir, metas) in &by_dir {
        let mut metas = metas.clone();
        // Most-specific-wins: highest specificity, then name. Row 0 becomes the primary.
        metas.sort_by(|a, b| b.specificity.cmp(&a.specificity).then(a.name.cmp(&b.name)));
        // Dedup by kind (keep the first = highest specificity) so two library defs sharing a
        // `kind` can't collide on the (path, app_kind) primary key.
        let mut seen: HashSet<&str> = HashSet::new();
        metas.retain(|d| seen.insert(d.kind.as_str()));
        let apps: Vec<DetectedApp> = metas
            .iter()
            .enumerate()
            .map(|(i, d)| DetectedApp {
                path: dir.clone(),
                app_kind: d.kind.clone(),
                app_name: d.name.clone(),
                family: d.family.clone(),
                specificity: d.specificity,
                is_primary: i == 0,
                markers_json: serde_json::to_string(&d.markers).unwrap_or_else(|_| "[]".into()),
                source: "builtin".to_owned(),
                detected_at: 0,
            })
            .collect();
        written += apps.len();
        store.replace_apps_for_dir(dir, &apps)?;
    }

    // Clear directories that no longer match any fingerprint (empty replace = delete).
    for stale in existing.difference(&matched) {
        store.replace_apps_for_dir(stale, &[])?;
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::{Entry, EntryKind};

    fn entry(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: path.into(),
            kind,
            size: 0,
            modified: None,
            hint: None,
            is_binary: false,
        }
    }
    fn dir(path: &str) -> Entry {
        entry(path, EntryKind::Dir)
    }
    fn file(path: &str) -> Entry {
        entry(path, EntryKind::File)
    }

    #[test]
    fn detects_persists_and_self_corrects() {
        let mut store = Store::open_in_memory().unwrap();
        // A Next.js app under /repo/web.
        store
            .upsert_entries(&[
                dir("/repo"),
                dir("/repo/web"),
                file("/repo/web/package.json"),
                file("/repo/web/next.config.js"),
            ])
            .unwrap();
        let defs = fingerprint::load(None).unwrap();

        let n = detect_directory_apps(&mut store, &defs).unwrap();
        assert!(n >= 2); // Node + Next.js both recorded

        let primary = store.primary_app_for_dir("/repo/web").unwrap().unwrap();
        assert_eq!(primary.app_kind, "nextjs_app"); // most-specific wins

        // Now the folder stops being Next.js (drop next.config.js). Re-detect must self-correct.
        store.delete_entry("/repo/web/next.config.js").unwrap();
        detect_directory_apps(&mut store, &defs).unwrap();
        let primary = store.primary_app_for_dir("/repo/web").unwrap().unwrap();
        assert_eq!(primary.app_kind, "node_package");
        assert!(store
            .apps_for_dir("/repo/web")
            .unwrap()
            .iter()
            .all(|a| a.app_kind != "nextjs_app"));
    }
}
