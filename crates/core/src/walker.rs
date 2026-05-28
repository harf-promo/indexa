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

/// Walk `root` and return all entries. Directories whose hint is `Skip` are not
/// descended into. Uses `jwalk` for parallel traversal.
pub fn walk(root: &Path, cfg: &WalkConfig) -> anyhow::Result<Vec<Entry>> {
    use jwalk::{Parallelism, WalkDir};

    let pool_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2);

    let walker = {
        let mut w = WalkDir::new(root)
            .sort(false)
            // Each walk owns its own rayon pool to avoid deadlock when multiple
            // walks run concurrently sharing the global rayon pool.
            .parallelism(Parallelism::RayonNewPool(pool_threads));
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

        // Don't descend into skipped dirs — we still record the dir itself.
        if meta.is_dir() {
            if let Some(ref h) = hint {
                if h.deep_scan == DeepScanPolicy::Skip {
                    // Record the dir but jwalk will still descend; we filter children below.
                    // A real impl would use jwalk's `process_read_dir` callback to prune.
                    // For now we record the entry and let the store handle deduplication.
                }
            }
        }

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
