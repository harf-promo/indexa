use hyperpolyglot::LanguageType;
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
    /// Credential / private-key store — skip by default; opt in via `[scan] include_sensitive`.
    Sensitive,
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
        // A Cargo build dir: marked by Cargo's own `CACHEDIR.TAG`, OR simply named `target`
        // next to a `Cargo.toml` (the tag is absent in many real cases — partial builds, test
        // fixtures, copied trees — and missing it indexed 100k+ `.o` files in the wild).
        |p| {
            ends_with(p, "target")
                && (p.join("CACHEDIR.TAG").exists()
                    || p.parent()
                        .is_some_and(|par| par.join("Cargo.toml").exists()))
        },
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
                && p.parent().is_some_and(|par| {
                    par.join("package.json").exists()
                        || par.join("setup.py").exists()
                        || par.join("pyproject.toml").exists()
                })
        },
        PathHint {
            label: "JS/Python build output",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        // CocoaPods vendored pods — skip only when a `Podfile` sits alongside, so a
        // hand-written directory that merely happens to be named `Pods` isn't pruned.
        |p| ends_with(p, "Pods") && p.parent().is_some_and(|par| par.join("Podfile").exists()),
        PathHint {
            label: "CocoaPods dependencies",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        // Vendored dependencies (Go modules / PHP Composer / Ruby Bundler) — skip only
        // next to the manifest that generates them, never a committed source `vendor/`.
        |p| {
            ends_with(p, "vendor")
                && p.parent().is_some_and(|par| {
                    par.join("go.mod").exists()
                        || par.join("composer.json").exists()
                        || par.join("Gemfile").exists()
                })
        },
        PathHint {
            label: "Vendored dependencies",
            category: "build-artifact",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        // Generic `build/` output — skip only next to a recognized build system's manifest,
        // so a project that keeps source in a `build/` directory is never pruned.
        |p| {
            ends_with(p, "build")
                && p.parent().is_some_and(|par| {
                    par.join("CMakeLists.txt").exists()
                        || par.join("Makefile").exists()
                        || par.join("meson.build").exists()
                        || par.join("build.gradle").exists()
                        || par.join("pom.xml").exists()
                })
        },
        PathHint {
            label: "Build output",
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
    // ── Linux XDG base dirs (XDG_DATA_HOME / STATE_HOME / user bin) ─────────
    (
        |p| home_subdir(p, ".local/share"),
        PathHint {
            label: "User data (XDG_DATA_HOME)",
            category: "data",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    (
        |p| home_subdir(p, ".local/state"),
        PathHint {
            label: "App state (XDG_STATE_HOME)",
            category: "cache",
            deep_scan: DeepScanPolicy::Skip,
        },
    ),
    (
        |p| home_subdir(p, ".local/bin"),
        PathHint {
            label: "User binaries",
            category: "applications",
            deep_scan: DeepScanPolicy::StructureOnly,
        },
    ),
    // ── Sensitive credential / key stores — skip by default ──────────────
    // These must appear BEFORE the broader Library/* catch-alls so they win the
    // first-match lookup and are never accidentally promoted to StructureOnly.
    (
        |p| home_subdir(p, ".ssh"),
        PathHint {
            label: "SSH keys",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| home_subdir(p, ".gnupg"),
        PathHint {
            label: "GPG keyring",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| home_subdir(p, ".aws"),
        PathHint {
            label: "AWS credentials",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| path_contains(p, "Library/Keychains"),
        PathHint {
            label: "macOS Keychains",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    // ── Browser profiles (saved passwords, cookies, session tokens) ───────
    (
        |p| path_contains(p, "Application Support/Google/Chrome"),
        PathHint {
            label: "Chrome profile",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| path_contains(p, "Application Support/BraveSoftware"),
        PathHint {
            label: "Brave profile",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| path_contains(p, "Application Support/Firefox"),
        PathHint {
            label: "Firefox profile",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| path_contains(p, "Application Support/com.apple.Safari"),
        PathHint {
            label: "Safari data",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| home_subdir(p, ".mozilla"),
        PathHint {
            label: "Mozilla profile",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    // ── Password managers ─────────────────────────────────────────────────
    (
        |p| home_subdir(p, ".password-store"),
        PathHint {
            label: "pass password store",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
        },
    ),
    (
        |p| path_contains(p, "Group Containers/2BUA8C4S2C.com.agilebits"),
        PathHint {
            label: "1Password data",
            category: "sensitive",
            deep_scan: DeepScanPolicy::Sensitive,
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
            // Web + markup + schema (mime_guess misses many of these)
            "html" | "htm" | "css" | "scss" | "sass" | "less" | "vue" | "svelte" | "astro"
            | "graphql" | "gql" | "proto" | "json5" | "jsonc" => Some("code"),
            // Languages mime_guess/Linguist commonly misclassify as octet-stream
            "sol" | "jl" | "lua" | "clj" | "cljs" | "edn" | "ex" | "exs" | "erl" | "hrl" | "hs"
            | "ml" | "mli" | "scala" | "kt" | "kts" | "swift" | "dart" | "nim" | "zig" | "rmd" => {
                Some("code")
            }
            // Tabular / scientific data
            "csv" | "tsv" | "jsonl" | "ndjson" | "avro" | "orc" | "feather" | "npy" | "npz"
            | "h5" | "hdf5" | "pkl" | "pickle" | "dta" | "sav" => Some("data"),
            // Logs
            "log" => Some("logs"),
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
    if let Some(mime) = MimeGuess::from_path(path).first() {
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
            _ => {
                // No MIME match — fall through to Linguist-based detection below.
                ""
            }
        };
        if !category.is_empty() {
            return Some(PathHint {
                label: "file",
                category,
                deep_scan: DeepScanPolicy::Index,
            });
        }
    }

    // Linguist-based fallback (hyperpolyglot): covers 700+ languages by filename,
    // extension, shebang, and content heuristics. Only reads the file for genuinely
    // ambiguous extensions (.h, .m, .pl, etc.).
    let lang = hyperpolyglot::detect(path).ok().flatten()?;
    let lang_info = hyperpolyglot::Language::try_from(lang.language()).ok()?;
    let category = match lang_info.language_type {
        LanguageType::Programming | LanguageType::Markup => "code",
        LanguageType::Data => "data",
        LanguageType::Prose => "documents",
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

    #[test]
    fn sensitive_paths_classified_as_sensitive() {
        // These paths should never be descended into by default; they carry
        // private keys, tokens, and saved browser passwords.
        for path in [
            "/Users/testuser/Library/Keychains",
            "/Users/testuser/Library/Application Support/Google/Chrome",
            "/Users/testuser/Library/Application Support/BraveSoftware",
            "/Users/testuser/Library/Application Support/Firefox",
            "/Users/testuser/Library/Application Support/com.apple.Safari",
            "/home/testuser/.password-store",
        ] {
            let p = PathBuf::from(path);
            if let Some(hint) = classify(&p) {
                assert_eq!(
                    hint.deep_scan,
                    DeepScanPolicy::Sensitive,
                    "{path} should be Sensitive, got {:?}",
                    hint.deep_scan
                );
            }
            // If classify returns None it means the path matches no HINT — which is fine
            // for paths outside the user's real home dir; the home-subdir predicates are
            // live-home-dir checks, so only the path_contains-based ones fire here.
        }
    }

    #[test]
    fn extended_extensions_reduce_unknown_bucket() {
        // Extensions added to shrink the "unknown" category (#21).
        for (name, want) in [
            ("app/style.scss", "code"),
            ("api/schema.graphql", "code"),
            ("src/main.zig", "code"),
            ("data/export.csv", "data"),
            ("ml/model.h5", "data"),
            ("var/server.log", "logs"),
        ] {
            let h = classify_file_by_extension(&PathBuf::from(name))
                .unwrap_or_else(|| panic!("{name} should classify"));
            assert_eq!(h.category, want, "{name}");
        }
    }
}
