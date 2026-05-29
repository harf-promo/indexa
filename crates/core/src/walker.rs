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

#[derive(Default)]
pub struct WalkConfig {
    /// Skip hidden files/dirs (dot-prefixed on Unix).
    pub skip_hidden: bool,
    /// Maximum directory depth (None = unlimited).
    pub max_depth: Option<usize>,
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

    let walker = {
        let mut w = WalkDir::new(root)
            .sort(false)
            // Each walk owns its own rayon pool to avoid deadlock when multiple
            // walks run concurrently sharing the global rayon pool.
            .parallelism(Parallelism::RayonNewPool(pool_threads))
            // Prune at read-dir time: stop jwalk from descending into Skip dirs
            // (and hidden dirs when requested). The dir entry itself is still
            // yielded; we just don't read its children.
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
                    if hidden || is_skip_dir(&cp) {
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
}
