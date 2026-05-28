use mime_guess::MimeGuess;
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
            p.to_str().is_some_and(|s| s.contains("/.var/app"))
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

/// Extension/filename-based classification for individual files.
/// Runs after `classify()` returns `None` so directory rules still win.
pub fn classify_file_by_extension(path: &Path) -> Option<PathHint> {
    // Well-known filenames with no extension.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let cat = match name {
            "Makefile" | "GNUmakefile" | "Rakefile" | "Gemfile" | "Dockerfile"
            | "Containerfile" | "Justfile" | "justfile" | "Vagrantfile" => Some("code"),
            "LICENSE" | "LICENCE" | "NOTICE" | "COPYING" | "AUTHORS" | "CONTRIBUTORS"
            | "README" | "CHANGELOG" | "CHANGES" | "INSTALL" | "TODO" | "FIXME" => {
                Some("documents")
            }
            ".env" | ".gitignore" | ".gitattributes" | ".gitmodules" | ".dockerignore"
            | ".editorconfig" | ".npmrc" | ".yarnrc" | ".nvmrc" | ".node-version"
            | ".python-version" | ".ruby-version" | ".tool-versions" => Some("config"),
            // .env.* variants (e.g. .env.local, .env.production)
            n if n.starts_with(".env.") => Some("config"),
            "Cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "go.sum"
            | "poetry.lock" | "Pipfile.lock" | "Gemfile.lock" | "composer.lock" | "mix.lock"
            | "flake.lock" => Some("lockfile"),
            _ => None,
        };
        if let Some(c) = cat {
            return Some(PathHint {
                label: "file",
                category: c,
                deep_scan: DeepScanPolicy::Index,
            });
        }
    }

    // Extension-based explicit overrides (before MIME fallback).
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let cat = match ext.to_lowercase().as_str() {
            // Config files
            "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "env" | "properties" | "plist"
            | "hcl" | "tf" | "tfvars" => Some("config"),
            // Dot-config patterns (e.g. .env.production)
            "env.local" | "env.production" | "env.development" | "env.test" => Some("config"),
            // Lockfiles
            "lock" | "sum" => Some("lockfile"),
            // Data files
            "db" | "sqlite" | "sqlite3" | "duckdb" | "parquet" | "arrow" | "lance" | "mdb"
            | "accdb" => Some("data"),
            // Archive/compressed
            "zip" | "tar" | "gz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "br" | "lz4" | "lzma"
            | "cab" | "iso" | "dmg" | "pkg" => Some("archive"),
            // Fonts
            "ttf" | "otf" | "woff" | "woff2" | "eot" | "pfb" | "pfm" | "afm" => Some("font"),
            // Notebooks
            "ipynb" => Some("code"),
            // Scripts (broad)
            "sh" | "bash" | "zsh" | "fish" | "ksh" | "csh" | "tcsh" | "ps1" | "psm1" | "psd1"
            | "bat" | "cmd" | "vbs" => Some("code"),
            _ => None,
        };
        if let Some(c) = cat {
            return Some(PathHint {
                label: "file",
                category: c,
                deep_scan: DeepScanPolicy::Index,
            });
        }
    }

    // MIME-based fallback.
    let mime = MimeGuess::from_path(path).first()?;
    let category = match (mime.type_().as_str(), mime.subtype().as_str()) {
        ("image", _) | ("audio", _) | ("video", _) => "media",
        ("font", _) => "font",
        ("text", _) => "code",
        ("application", "pdf")
        | ("application", "msword")
        | ("application", "epub+zip")
        | ("application", "vnd.oasis.opendocument.text")
        | ("application", "vnd.oasis.opendocument.spreadsheet")
        | ("application", "vnd.oasis.opendocument.presentation") => "documents",
        ("application", sub)
            if sub.starts_with("vnd.openxmlformats-officedocument")
                || sub.starts_with("vnd.ms-") =>
        {
            "documents"
        }
        ("application", "zip")
        | ("application", "x-tar")
        | ("application", "gzip")
        | ("application", "x-bzip")
        | ("application", "x-bzip2")
        | ("application", "x-7z-compressed")
        | ("application", "x-rar-compressed")
        | ("application", "x-xz")
        | ("application", "zstd") => "archive",
        ("application", "json")
        | ("application", "toml")
        | ("application", "xml")
        | ("application", "x-yaml") => "code",
        ("application", "x-sqlite3") | ("application", "vnd.sqlite3") => "data",
        _ => return None,
    };
    Some(PathHint {
        label: "file",
        category,
        deep_scan: DeepScanPolicy::Index,
    })
}

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

    #[test]
    fn pdf_outside_documents_classified_as_documents() {
        let p = PathBuf::from("/home/user/projects/report.pdf");
        let hint = classify_file_by_extension(&p).unwrap();
        assert_eq!(hint.category, "documents");
    }

    #[test]
    fn env_file_classified_as_config() {
        for name in [
            ".env",
            ".env.local",
            "app.toml",
            "settings.yaml",
            "config.yml",
            "server.ini",
        ] {
            let p = PathBuf::from(format!("/home/user/project/{name}"));
            let hint = classify_file_by_extension(&p).unwrap();
            assert_eq!(hint.category, "config", "failed for {name}");
        }
    }

    #[test]
    fn lockfile_classified() {
        for name in [
            "Cargo.lock",
            "package-lock.json",
            "yarn.lock",
            "go.sum",
            "poetry.lock",
        ] {
            let p = PathBuf::from(format!("/home/user/project/{name}"));
            let hint = classify_file_by_extension(&p).unwrap();
            assert_eq!(hint.category, "lockfile", "failed for {name}");
        }
    }

    #[test]
    fn media_files_classified() {
        for name in [
            "photo.jpg",
            "video.mp4",
            "song.mp3",
            "image.heic",
            "clip.mov",
        ] {
            let p = PathBuf::from(format!("/home/user/files/{name}"));
            let hint = classify_file_by_extension(&p).unwrap();
            assert_eq!(hint.category, "media", "failed for {name}");
        }
    }

    #[test]
    fn archive_files_classified() {
        for name in ["backup.zip", "release.tar.gz", "archive.7z"] {
            let p = PathBuf::from(format!("/home/user/files/{name}"));
            let hint = classify_file_by_extension(&p).unwrap();
            assert_eq!(hint.category, "archive", "failed for {name}");
        }
    }

    #[test]
    fn font_files_classified() {
        for name in ["font.ttf", "font.otf", "font.woff2"] {
            let p = PathBuf::from(format!("/home/user/fonts/{name}"));
            let hint = classify_file_by_extension(&p).unwrap();
            assert_eq!(hint.category, "font", "failed for {name}");
        }
    }

    #[test]
    fn data_files_classified() {
        for name in ["db.sqlite", "data.parquet", "app.db"] {
            let p = PathBuf::from(format!("/home/user/data/{name}"));
            let hint = classify_file_by_extension(&p).unwrap();
            assert_eq!(hint.category, "data", "failed for {name}");
        }
    }

    #[test]
    fn well_known_filenames_classified() {
        let p = PathBuf::from("/home/user/project/Makefile");
        assert_eq!(classify_file_by_extension(&p).unwrap().category, "code");
        let p = PathBuf::from("/home/user/project/Dockerfile");
        assert_eq!(classify_file_by_extension(&p).unwrap().category, "code");
        let p = PathBuf::from("/home/user/project/LICENSE");
        assert_eq!(
            classify_file_by_extension(&p).unwrap().category,
            "documents"
        );
        let p = PathBuf::from("/home/user/project/.gitignore");
        assert_eq!(classify_file_by_extension(&p).unwrap().category, "config");
    }

    #[test]
    fn unknown_extension_returns_none() {
        let p = PathBuf::from("/home/user/mystery.xyzabc123");
        assert!(classify_file_by_extension(&p).is_none());
    }
}
