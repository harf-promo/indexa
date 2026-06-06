use crate::surface::{classify, classify_file_by_extension, DeepScanPolicy, PathHint};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hint: Option<PathHint>,
}

pub struct WalkConfig {
    /// Skip hidden files/dirs (dot-prefixed on Unix).
    pub skip_hidden: bool,
    /// Maximum directory depth (None = unlimited).
    pub max_depth: Option<usize>,
    /// Honor the scan root's `.gitignore` (its patterns, anchored at the root).
    pub respect_gitignore: bool,
    /// Extra gitignore-style patterns to skip (from `[scan] ignore`).
    pub ignore: Vec<String>,
}

impl Default for WalkConfig {
    fn default() -> Self {
        Self {
            skip_hidden: false,
            max_depth: None,
            // Default on: a scan respects the repo's .gitignore unless a caller opts out.
            respect_gitignore: true,
            ignore: Vec::new(),
        }
    }
}

/// Build a gitignore-style matcher for a scan `root` from its `.gitignore` (when
/// `respect_gitignore`) plus any `[scan] ignore` patterns. Returns `None` when there is
/// nothing to match (so the hot path skips the check entirely). Patterns are anchored at
/// `root`; nested per-subdirectory `.gitignore` files are not separately loaded.
fn build_ignore_matcher(root: &Path, cfg: &WalkConfig) -> Option<ignore::gitignore::Gitignore> {
    if !cfg.respect_gitignore && cfg.ignore.is_empty() {
        return None;
    }
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    if cfg.respect_gitignore {
        let gi = root.join(".gitignore");
        if gi.is_file() {
            let _ = b.add(gi); // add() returns Some(err) on a bad file — ignore, build empty
        }
    }
    for pat in &cfg.ignore {
        let _ = b.add_line(None, pat);
    }
    b.build().ok()
}

/// True if `dir_path` is a directory we should never descend into — build
/// artifacts (`target/`, `node_modules/`), VCS internals (`.git/`), caches, etc.
/// Centralises the "don't waste time indexing generated files" decision so both
/// the walker prune callback and any caller can share it.
pub fn is_skip_dir(dir_path: &Path) -> bool {
    classify(dir_path)
        .map(|h| h.deep_scan == DeepScanPolicy::Skip)
        .unwrap_or(false)
}

/// Walk `root` and return all entries. Directories classified `Skip` (build
/// artifacts, caches, VCS internals) are recorded but **not descended into**, so
/// we never index the thousands of generated files inside `target/`,
/// `node_modules/`, `.git/`, etc. Uses `jwalk` for parallel traversal and prunes
/// via the `process_read_dir` callback.
pub fn walk(root: &Path, cfg: &WalkConfig) -> anyhow::Result<Vec<Entry>> {
    use jwalk::{Parallelism, WalkDir};

    let pool_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2);

    let skip_hidden = cfg.skip_hidden;
    // .gitignore + `[scan] ignore` matcher (None when there's nothing to match).
    let matcher = build_ignore_matcher(root, cfg).map(std::sync::Arc::new);
    let cb_matcher = matcher.clone();

    let walker = {
        let mut w = WalkDir::new(root)
            .sort(false)
            // Each walk owns its own rayon pool to avoid deadlock when multiple
            // walks run concurrently sharing the global rayon pool.
            .parallelism(Parallelism::RayonNewPool(pool_threads))
            // Prune at read-dir time: stop jwalk from descending into Skip dirs
            // (and hidden / gitignored dirs). The dir entry itself is still yielded
            // here; the main loop drops gitignored entries so they aren't recorded.
            .process_read_dir(move |_depth, _path, _state, children| {
                for child in children.iter_mut().flatten() {
                    if !child.file_type().is_dir() {
                        continue;
                    }
                    let cp = child.path();
                    let hidden = skip_hidden
                        && child
                            .file_name()
                            .to_str()
                            .map(|n| n.starts_with('.'))
                            .unwrap_or(false);
                    let ignored = cb_matcher
                        .as_ref()
                        .is_some_and(|m| m.matched(&cp, true).is_ignore());
                    if hidden || is_skip_dir(&cp) || ignored {
                        // Prevent descending into this directory.
                        child.read_children_path = None;
                    }
                }
            });
        if let Some(d) = cfg.max_depth {
            w = w.max_depth(d);
        }
        w
    };

    let mut entries = Vec::new();

    for result in walker {
        let entry = result?;
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Drop gitignored / `[scan] ignore`-matched entries (an ignored dir is recorded by
        // jwalk before pruning; this also keeps its own row out of the index).
        if let Some(m) = &matcher {
            if m.matched(&path, meta.is_dir()).is_ignore() {
                continue;
            }
        }

        if cfg.skip_hidden {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }
            }
        }

        let hint = classify(&path).or_else(|| {
            if meta.is_file() {
                classify_file_by_extension(&path)
            } else {
                None
            }
        });

        let kind = if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };

        entries.push(Entry {
            path,
            kind,
            size: if meta.is_file() { meta.len() } else { 0 },
            modified: meta.modified().ok(),
            hint,
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), "world").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c.txt"), "!").unwrap();

        let entries = walk(dir.path(), &WalkConfig::default()).unwrap();
        let files: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .collect();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn prunes_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "keep me").unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir(&nm).unwrap();
        std::fs::write(nm.join("dep.js"), "generated").unwrap();
        std::fs::create_dir(nm.join("nested")).unwrap();
        std::fs::write(nm.join("nested").join("more.js"), "more").unwrap();

        let entries = walk(dir.path(), &WalkConfig::default()).unwrap();
        // The node_modules dir itself is recorded, but nothing inside it is.
        let has_dep = entries.iter().any(|e| e.path.ends_with("dep.js"));
        let has_nested = entries.iter().any(|e| e.path.ends_with("more.js"));
        let has_real = entries.iter().any(|e| e.path.ends_with("real.txt"));
        assert!(!has_dep, "node_modules contents must not be indexed");
        assert!(
            !has_nested,
            "nested node_modules contents must not be indexed"
        );
        assert!(has_real, "real files must still be indexed");
    }

    #[test]
    fn prunes_cargo_target_without_cachedir_tag() {
        // A Cargo `target/` whose `CACHEDIR.TAG` is absent (test fixtures, partial builds, copied
        // trees) must still be pruned — recognized by a sibling `Cargo.toml`. Regression for the
        // bug where 100k+ `.o`/`.bin` build artifacts were indexed and queued for summarization.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn main() {}").unwrap();
        let tgt = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&tgt).unwrap();
        std::fs::write(tgt.join("foo.o"), "binary").unwrap();
        std::fs::write(tgt.join("app.bin"), "binary").unwrap();
        // No CACHEDIR.TAG is written.

        let entries = walk(dir.path(), &WalkConfig::default()).unwrap();
        assert!(
            !entries.iter().any(|e| e.path.ends_with("foo.o")),
            "target/ build artifacts must not be indexed (no CACHEDIR.TAG, sibling Cargo.toml)"
        );
        assert!(
            !entries.iter().any(|e| e.path.ends_with("app.bin")),
            "target/ build artifacts must not be indexed"
        );
        assert!(
            entries.iter().any(|e| e.path.ends_with("lib.rs")),
            "real source files must still be indexed"
        );
    }

    #[test]
    fn respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("deep.txt"), "deep").unwrap();

        let cfg = WalkConfig {
            max_depth: Some(1),
            ..Default::default()
        };
        let entries = walk(dir.path(), &cfg).unwrap();
        // depth=1 means only root + immediate children; deep.txt is at depth 2
        let has_deep = entries.iter().any(|e| e.path.ends_with("deep.txt"));
        assert!(!has_deep);
    }

    #[test]
    fn respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "secret.txt\nbuild/\n").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "kept").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "ignored").unwrap();
        let build = dir.path().join("build");
        std::fs::create_dir(&build).unwrap();
        std::fs::write(build.join("out.o"), "artifact").unwrap();

        let entries = walk(dir.path(), &WalkConfig::default()).unwrap();
        assert!(
            entries.iter().any(|e| e.path.ends_with("keep.rs")),
            "non-ignored files are still indexed"
        );
        assert!(
            !entries.iter().any(|e| e.path.ends_with("secret.txt")),
            ".gitignore'd file must be skipped"
        );
        assert!(
            !entries.iter().any(|e| e.path.ends_with("out.o")),
            ".gitignore'd directory's contents must be skipped"
        );

        // With respect_gitignore off, the ignored entries reappear.
        let cfg = WalkConfig {
            respect_gitignore: false,
            ..Default::default()
        };
        let entries = walk(dir.path(), &cfg).unwrap();
        assert!(entries.iter().any(|e| e.path.ends_with("secret.txt")));
    }

    #[test]
    fn respects_config_ignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rs"), "code").unwrap();
        std::fs::write(dir.path().join("debug.log"), "noise").unwrap();
        let vendor = dir.path().join("vendor");
        std::fs::create_dir(&vendor).unwrap();
        std::fs::write(vendor.join("lib.rs"), "third party").unwrap();

        let cfg = WalkConfig {
            respect_gitignore: false,
            ignore: vec!["*.log".into(), "vendor/".into()],
            ..Default::default()
        };
        let entries = walk(dir.path(), &cfg).unwrap();
        assert!(entries.iter().any(|e| e.path.ends_with("app.rs")));
        assert!(
            !entries.iter().any(|e| e.path.ends_with("debug.log")),
            "config `ignore` glob must skip *.log"
        );
        assert!(
            !entries.iter().any(|e| e.path.ends_with("lib.rs")),
            "config `ignore` must skip vendor/ contents"
        );
    }
}
