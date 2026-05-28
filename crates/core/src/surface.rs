use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepScanPolicy {
    /// Parse, describe, and embed this region.
    Index,
    /// Walk for structure but skip content extraction (e.g. app bundles).
    StructureOnly,
    /// Skip entirely — build artifacts, caches, etc.
    Skip,
}

#[derive(Debug, Clone)]
pub struct PathHint {
    pub label: &'static str,
    pub category: &'static str,
    pub deep_scan: DeepScanPolicy,
}

/// Returns the first hint whose predicate matches `path`, or `None`.
pub fn classify(path: &Path) -> Option<PathHint> {
    HINTS
        .iter()
        .find(|(pred, _)| pred(path))
        .map(|(_, hint)| hint.clone())
}

type Predicate = fn(&Path) -> bool;

static HINTS: &[(Predicate, PathHint)] = &[
    // ── Build artifacts / caches — skip ──────────────────────────────────
    (
        |p| ends_with(p, "node_modules"),
        PathHint {
            label: "Node.js dependencies",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| ends_with(p, "target") && p.join("CACHEDIR.TAG").exists(),
        PathHint {
            label: "Rust build output",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| ends_with(p, ".venv") || ends_with(p, "venv") || ends_with(p, ".virtualenv"),
        PathHint {
            label: "Python virtual environment",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| ends_with(p, "__pycache__"),
        PathHint {
            label: "Python bytecode cache",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| ends_with(p, ".gradle"),
        PathHint {
            label: "Gradle cache",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| ends_with(p, ".next"),
        PathHint {
            label: "Next.js build cache",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| {
            ends_with(p, "dist")
                && p.parent()
                    .is_some_and(|par| par.join("package.json").exists())
        },
        PathHint {
            label: "JS/TS build output",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    // ── System / virtual filesystems — skip ──────────────────────────────
    (
        |p| {
            let s = p.to_str().unwrap_or("");
            s == "/proc" || s == "/sys" || s == "/dev" || s == "/run" || s == "/tmp"
        },
        PathHint {
            label: "Virtual filesystem",
            category: "system",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    // ── Linux XDG cache + snap/flatpak — skip ─────────────────────────────
    (
        |p| home_subdir(p, ".cache"),
        PathHint {
            label: "User cache",
            category: "cache",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| home_subdir(p, "snap"),
        PathHint {
            label: "Snap packages",
            category: "apps",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| {
            // ~/.var/app is the Flatpak per-app data directory
            p.to_str()
                .is_some_and(|s| s.contains("/.var/app"))
        },
        PathHint {
            label: "Flatpak app data",
            category: "apps",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    // ── Linux XDG config — structure only ─────────────────────────────────
    (
        |p| home_subdir(p, ".config"),
        PathHint {
            label: "User config files",
            category: "config",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    // ── macOS system data ──────────────────────────────────────────────────
    (
        |p| path_contains(p, "Library/Caches"),
        PathHint {
            label: "macOS caches",
            category: "system",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| path_contains(p, "Library/Application Support"),
        PathHint {
            label: "macOS app support data",
            category: "system",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    (
        |p| path_contains(p, ".local/share/Trash") || path_contains(p, ".Trash"),
        PathHint {
            label: "Trash",
            category: "system",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    // ── Installed applications ────────────────────────────────────────────
    (
        |p| {
            (p.to_str() == Some("/Applications"))
                || p.to_str().is_some_and(|s| {
                    s.ends_with("/Applications")
                        && p.parent()
                            .is_some_and(|par| par.to_str() == Some(&dirs_home()))
                })
        },
        PathHint {
            label: "Installed applications",
            category: "applications",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    // ── Code projects — manifest-based fingerprints ───────────────────────
    // These run BEFORE the generic .git matcher so the manifest label wins.
    (
        has_cargo_sibling,
        PathHint {
            label: "Rust project",
            category: "code",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        has_pkg_json_sibling,
        PathHint {
            label: "JavaScript/TypeScript project",
            category: "code",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        has_pyproject_sibling,
        PathHint {
            label: "Python project",
            category: "code",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        |p| p.join(".git").is_dir(),
        PathHint {
            label: "Git repository",
            category: "code",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    // ── Well-known user directories ───────────────────────────────────────
    (
        |p| home_subdir(p, "Documents"),
        PathHint {
            label: "Documents",
            category: "documents",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        |p| home_subdir(p, "Downloads"),
        PathHint {
            label: "Downloads",
            category: "scratch",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        |p| home_subdir(p, "Desktop"),
        PathHint {
            label: "Desktop",
            category: "scratch",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        |p| home_subdir(p, "Pictures"),
        PathHint {
            label: "Pictures",
            category: "media",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        |p| home_subdir(p, "Movies") || home_subdir(p, "Videos"),
        PathHint {
            label: "Videos",
            category: "media",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    (
        |p| home_subdir(p, "Music"),
        PathHint {
            label: "Music",
            category: "media",
            deep_scan: DeepScanPolicy::Index,
        },
    ),
    // ── Creative app libraries ────────────────────────────────────────────
    (
        |p| p.to_str().is_some_and(|s| s.ends_with(".photoslibrary")),
        PathHint {
            label: "Photos Library",
            category: "media",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    (
        |p| p.to_str().is_some_and(|s| s.ends_with(".fcpbundle")),
        PathHint {
            label: "Final Cut Pro library",
            category: "media",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    (
        |p| {
            p.to_str()
                .is_some_and(|s| s.ends_with(".lrcat") || s.ends_with(".lrdata"))
        },
        PathHint {
            label: "Lightroom catalog",
            category: "media",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
];

// ── Predicate helpers ─────────────────────────────────────────────────────────

fn ends_with(path: &Path, segment: &str) -> bool {
    path.file_name().is_some_and(|n| n == segment)
}

fn path_contains(path: &Path, fragment: &str) -> bool {
    path.to_str().is_some_and(|s| s.contains(fragment))
}

fn dirs_home() -> String {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn home_subdir(path: &Path, name: &str) -> bool {
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
        path == home.join(name)
    } else {
        false
    }
}

// Each manifest gets its own fn because `Predicate = fn(&Path)` cannot capture.
fn has_sibling(p: &Path, name: &str) -> bool {
    p.parent().is_some_and(|par| par.join(name).exists())
}

fn has_cargo_sibling(p: &Path) -> bool {
    has_sibling(p, "Cargo.toml")
}

fn has_pkg_json_sibling(p: &Path) -> bool {
    has_sibling(p, "package.json")
}

fn has_pyproject_sibling(p: &Path) -> bool {
    has_sibling(p, "pyproject.toml") || has_sibling(p, "setup.py")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn node_modules_skipped() {
        let p = PathBuf::from("/home/user/project/node_modules");
        let hint = classify(&p).unwrap();
        assert_eq!(hint.deep_scan, DeepScanPolicy::Skip);
    }

    #[test]
    fn unknown_path_returns_none() {
        let p = PathBuf::from("/home/user/some/random/folder");
        assert!(classify(&p).is_none());
    }

    #[test]
    fn git_repo_indexed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let hint = classify(dir.path()).unwrap();
        assert_eq!(hint.deep_scan, DeepScanPolicy::Index);
        assert_eq!(hint.category, "code");
    }

    #[test]
    fn rust_project_manifest_detected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        // classify checks the *parent* of the directory — so we need a subdir
        let subdir = dir.path().join("src");
        std::fs::create_dir(&subdir).unwrap();
        let hint = classify(&subdir);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().label, "Rust project");
    }

    #[test]
    fn js_project_manifest_detected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let subdir = dir.path().join("src");
        std::fs::create_dir(&subdir).unwrap();
        let hint = classify(&subdir);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().label, "JavaScript/TypeScript project");
    }

    #[test]
    fn virtual_fs_skipped() {
        for path in ["/proc", "/sys", "/dev", "/run", "/tmp"] {
            let p = PathBuf::from(path);
            let hint = classify(&p).unwrap();
            assert_eq!(hint.deep_scan, DeepScanPolicy::Skip, "failed for {path}");
        }
    }
}
