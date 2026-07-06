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
    /// NUL-sniffed binary flag (only meaningful when [`WalkConfig::sniff_binary`] was set;
    /// always `false` otherwise). The deep phase skips files marked binary so a whole-computer
    /// scan doesn't try to parse executables/images/DB blobs. Metadata is still recorded.
    pub is_binary: bool,
}

#[derive(Debug, Clone)]
pub struct WalkConfig {
    /// Skip hidden files/dirs (dot-prefixed on Unix).
    pub skip_hidden: bool,
    /// Maximum directory depth (None = unlimited).
    pub max_depth: Option<usize>,
    /// Honor `.gitignore` files (root AND nested subdirectories) plus global gitignore
    /// and `.git/info/exclude`. Powered by the `ignore` crate — all nested ignore
    /// files are honoured, unlike the old root-only implementation.
    pub respect_gitignore: bool,
    /// Extra gitignore-style patterns to skip (from `[scan] ignore`).
    pub ignore: Vec<String>,
    /// Scan-time per-file size cap (bytes). Files larger than this are not yielded.
    /// `None` = no cap. Default: `Some(8 MiB)` — skips media blobs / VM images /
    /// large DB dumps that are never useful context.
    pub max_filesize: Option<u64>,
    /// Descend into `DeepScanPolicy::Sensitive` directories (`.ssh`, `.gnupg`, browser
    /// profiles, Keychains, etc.). Defaults to `false` — these credential/key stores are
    /// never walked unless a caller explicitly opts in.
    pub include_sensitive: bool,
    /// Sniff each file's first 8 KB for a NUL byte and set [`Entry::is_binary`] (ripgrep's
    /// heuristic). Off by default — adds one file-open per file. When on (`[scan] skip_binary`),
    /// the deep phase skips flagged binaries. Fail-open: an unreadable file is never flagged.
    pub sniff_binary: bool,
}

/// Default scan-time per-file size cap (8 MiB). Skips blobs that are almost
/// never useful context and dominate disk/index growth on a broad scan.
pub const DEFAULT_MAX_FILESIZE: u64 = 8 * 1024 * 1024;

impl Default for WalkConfig {
    fn default() -> Self {
        Self {
            skip_hidden: false,
            max_depth: None,
            // Default on: a scan respects the repo's .gitignore unless a caller opts out.
            respect_gitignore: true,
            ignore: Vec::new(),
            max_filesize: Some(DEFAULT_MAX_FILESIZE),
            include_sensitive: false,
            sniff_binary: false,
        }
    }
}

/// Directory names that are ALWAYS skipped, regardless of gitignore or `[scan]`
/// config — VCS internals, IDE state, and language-specific cache dirs that are
/// never useful context and can be enormous.
static ALWAYS_SKIP_DIR_NAMES: &[&str] = &[
    // Version-control internals
    ".git",
    ".svn",
    ".hg",
    ".jj",
    // Python caches / tool dirs
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    "__pycache__",
    // IDE / editor state
    ".idea",
    ".vscode",
    ".eclipse",
    ".metals",
    ".bloop",
    // Dart / Flutter
    ".dart_tool",
    ".pub-cache",
    // Misc caches
    ".cache",
    ".nx",
];

/// True if `dir_path` is a directory we should never descend into — VCS
/// internals (`.git/`), language/tool caches (`.pytest_cache`, `.mypy_cache`,
/// etc.), IDE state (`.idea/`), or build artifacts (`target/`, `node_modules/`).
///
/// The first check is a fast name-based lookup for dirs that are unconditionally
/// skipped. The second delegates to `classify()` for project-structure-aware
/// recognition of build output directories (e.g. `target/` adjacent to
/// `Cargo.toml`, `Pods/` adjacent to a `Podfile`).
///
/// Centralises the skip decision so both the walker prune callback and any
/// caller can share it.
pub fn is_skip_dir(dir_path: &Path) -> bool {
    // Fast path: well-known names that are never useful context.
    if let Some(name) = dir_path.file_name().and_then(|n| n.to_str()) {
        if ALWAYS_SKIP_DIR_NAMES.contains(&name) {
            return true;
        }
    }
    // Project-structure-aware classification (target/, node_modules/, Pods/, …).
    classify(dir_path)
        .map(|h| h.deep_scan == DeepScanPolicy::Skip)
        .unwrap_or(false)
}

/// True if `dir_path` is a credential/key store (`.ssh`, `.gnupg`, browser profiles,
/// Keychains, etc.) classified `DeepScanPolicy::Sensitive`. Such directories are excluded
/// from the walk unless `WalkConfig::include_sensitive` is set.
pub fn is_sensitive_dir(dir_path: &Path) -> bool {
    classify(dir_path)
        .map(|h| h.deep_scan == DeepScanPolicy::Sensitive)
        .unwrap_or(false)
}

/// True if `path` is itself a secret-bearing file (`.env`, private keys, `.pem`/keystores,
/// credential files) classified `DeepScanPolicy::Sensitive` by [`classify_file_by_extension`] —
/// the file-level analogue of [`is_sensitive_dir`]. Such files are recorded as metadata but not
/// deep-parsed or embedded unless `include_sensitive` is set, so their contents (which redaction
/// can't reliably scrub — e.g. a raw PEM) never enter the searchable index.
pub fn is_sensitive_file(path: &Path) -> bool {
    classify_file_by_extension(path)
        .map(|h| h.deep_scan == DeepScanPolicy::Sensitive)
        .unwrap_or(false)
}

/// Build a gitignore matcher for `root` combining the root `.gitignore` (when `respect_gitignore`)
/// and the `[scan] ignore` config patterns, both anchored at `root`. Returns `None` when there are
/// no rules. Shared by the walker prune callback and the watchers' per-event
/// [`should_index_file`] check, so file selection can't drift between a scan and a live watch.
pub fn build_ignore_matcher(
    root: &Path,
    respect_gitignore: bool,
    ignore: &[String],
) -> Option<ignore::gitignore::Gitignore> {
    let mut gb = ignore::gitignore::GitignoreBuilder::new(root);
    let mut has_rules = false;
    if respect_gitignore {
        let root_gi = root.join(".gitignore");
        if root_gi.is_file() {
            let _ = gb.add(&root_gi); // fail-open: a bad file is silently skipped
            has_rules = true;
        }
    }
    for pat in ignore {
        let _ = gb.add_line(None, pat); // fail-open: a bad pattern is silently skipped
        has_rules = true;
    }
    if has_rules {
        gb.build().ok()
    } else {
        None
    }
}

/// One [`build_ignore_matcher`] per root, for the watchers (which check individual event paths, not
/// a walk). Roots with no rules are omitted.
pub fn build_scan_matchers(
    roots: &[PathBuf],
    respect_gitignore: bool,
    ignore: &[String],
) -> Vec<(PathBuf, ignore::gitignore::Gitignore)> {
    roots
        .iter()
        .filter_map(|r| {
            build_ignore_matcher(r, respect_gitignore, ignore).map(|gi| (r.clone(), gi))
        })
        .collect()
}

/// Whether a single file event should be indexed — mirrors the walk's prune policy for the
/// watchers, which see individual paths rather than a prunable walk. Rejects the file when it
/// exceeds `max_filesize`, when any ancestor directory (up to its `roots` entry) is a skip-dir
/// (`target/`, `node_modules/`, `.git/`, …) or a sensitive dir (`.ssh/`, `.gnupg/`, …) with
/// `include_sensitive` off, when the file *itself* is a secret ([`is_sensitive_file`]: `.env`,
/// `id_rsa`, `*.pem`, …) with `include_sensitive` off, or when a `[scan] ignore`/gitignore rule in
/// `matchers` (from [`build_scan_matchers`]) matches. Without this a live watch re-indexes exactly
/// the build artifacts / credential stores / oversized blobs the scan walker deliberately skips.
pub fn should_index_file(
    path: &Path,
    roots: &[PathBuf],
    include_sensitive: bool,
    max_filesize: Option<u64>,
    matchers: &[(PathBuf, ignore::gitignore::Gitignore)],
) -> bool {
    // A symlink escapes the root: indexing follows the link and reads the target (outside the
    // root, past the size cap and deny-list). `is_symlink()` lstat's — it does not follow.
    if path.is_symlink() {
        return false;
    }
    if let Some(cap) = max_filesize {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > cap {
                return false;
            }
        }
    }
    for dir in crate::pathutil::ancestor_dirs_to_root(path, roots) {
        if is_skip_dir(&dir) {
            return false;
        }
        if !include_sensitive && is_sensitive_dir(&dir) {
            return false;
        }
    }
    // File-level secret (a `.env`/`id_rsa`/`*.pem` outside a known sensitive dir): metadata-only
    // unless the caller opted in. Mirrors the deep-phase gate so watch and scan agree.
    if !include_sensitive && is_sensitive_file(path) {
        return false;
    }
    for (root, gi) in matchers {
        if path.starts_with(root) && gi.matched(path, false).is_ignore() {
            return false;
        }
    }
    true
}

/// Fail-open NUL sniff: read a file's first 8 KB and check for a NUL byte (via
/// [`crate::text::is_binary`]). Returns `false` on any open/read error — an unreadable file is
/// never *classified* binary (so it isn't skipped on the strength of a read failure).
fn file_is_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8192];
    match f.read(&mut buf) {
        Ok(n) => crate::text::is_binary(&buf[..n]),
        Err(_) => false,
    }
}

/// Walk `root` and return all entries.
///
/// Uses `ignore::WalkBuilder` (ripgrep's parallel walker) so that **nested**
/// `.gitignore` files are honoured automatically — the old `jwalk` implementation
/// only loaded the root-level `.gitignore`, which caused build artifacts, `.git`
/// objects, and `node_modules` trees to leak into the index when projects had
/// per-subdirectory gitignore files.
///
/// Build-artifact / VCS / cache directories (`target/`, `node_modules/`, `.git/`,
/// `__pycache__`, `.idea`, …) are pruned by name via `is_skip_dir` even when no
/// `.gitignore` is present. In the parallel walk, `WalkState::Skip` prevents both
/// yielding the entry AND descending into its subtree.
pub fn walk(root: &Path, cfg: &WalkConfig) -> anyhow::Result<Vec<Entry>> {
    use ignore::{WalkBuilder, WalkState};
    use std::sync::{Arc, Mutex};

    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2);

    // Capture the fields we need in the `'static` parallel closure.
    let skip_hidden = cfg.skip_hidden;
    let include_sensitive = cfg.include_sensitive;
    let sniff_binary = cfg.sniff_binary;

    // Build a callback-side gitignore matcher.
    //
    // This serves two purposes:
    //
    // (1) Root .gitignore — WalkBuilder's `git_ignore(true)` only reads .gitignore
    //     files when the walked directory is inside a git repository (it detects
    //     a `.git` dir in an ancestor). Test fixtures and standalone project
    //     directories created outside any git repo are silently bypassed. Loading
    //     the root .gitignore explicitly via GitignoreBuilder ensures the patterns
    //     are always honoured, regardless of whether a `.git` ancestor exists.
    //     In a real git repo the patterns are applied twice (once by WalkBuilder,
    //     once by this matcher) — gitignore matching is idempotent, so this is safe.
    //
    // (2) `[scan] ignore` config patterns — extra gitignore-style globs from the
    //     user's config, anchored at root.
    // Root `.gitignore` + `[scan] ignore` patterns, anchored at root — shared with the watchers'
    // per-event `should_index_file` so a scan and a live watch select the same files.
    let combined_matcher: Option<Arc<ignore::gitignore::Gitignore>> =
        build_ignore_matcher(root, cfg.respect_gitignore, &cfg.ignore).map(Arc::new);

    let mut b = WalkBuilder::new(root);
    b.threads(threads)
        .follow_links(false)
        // Honor .gitignore files found during the walk (root AND nested) plus global
        // gitignore and .git/info/exclude. All gated on respect_gitignore.
        // For non-git directories, the root .gitignore is handled by combined_matcher.
        .git_ignore(cfg.respect_gitignore)
        .git_global(cfg.respect_gitignore)
        .git_exclude(cfg.respect_gitignore)
        .parents(cfg.respect_gitignore)
        // Also honor .ignore files (gitignore-style, not git-specific).
        .ignore(cfg.respect_gitignore)
        // WalkBuilder.hidden(true) skips dot-prefixed files/dirs natively.
        .hidden(cfg.skip_hidden)
        // Scan-time size cap: files above this are not yielded by the walker.
        .max_filesize(cfg.max_filesize);

    if let Some(d) = cfg.max_depth {
        b.max_depth(Some(d));
    }

    let entries: Arc<Mutex<Vec<Entry>>> = Arc::new(Mutex::new(Vec::new()));

    b.build_parallel().run({
        let entries = entries.clone();
        let combined_matcher = combined_matcher.clone();
        move || {
            let entries = entries.clone();
            let combined_matcher = combined_matcher.clone();
            Box::new(
                move |result: std::result::Result<ignore::DirEntry, ignore::Error>| {
                    let de = match result {
                        Ok(d) => d,
                        Err(_) => return WalkState::Continue, // fail-open: skip unreadable
                    };
                    let path = de.path();
                    let is_dir = de.file_type().map(|t| t.is_dir()).unwrap_or(false);

                    // Skip symlinks entirely. With follow_links(false) the walker lstat's a symlink
                    // as a size-0 File entry, but the deep phase OPENs the path — following the link
                    // — indexing content OUTSIDE the root (e.g. a `notes.txt` link to ~/.ssh/id_rsa)
                    // past both the size cap (0 bytes never trips it) and the sensitive deny-list
                    // (which keys on the link's own name, not its target). Guard on is_symlink(),
                    // NOT the name — an innocuously-named link would otherwise slip through.
                    if de.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                        return WalkState::Continue; // don't record; keep walking siblings
                    }

                    // Belt-and-suspenders hidden check: WalkBuilder.hidden(true) handles
                    // this, but guard in the callback too for robustness. Depth > 0 so we
                    // never accidentally prune the walk root itself.
                    if skip_hidden
                        && de.depth() > 0
                        && path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with('.'))
                    {
                        return if is_dir {
                            WalkState::Skip
                        } else {
                            WalkState::Continue
                        };
                    }

                    // Prune build-artifact / VCS / cache directories by name.
                    // `is_skip_dir` calls `classify()` which recognises target/, node_modules/,
                    // .git/, __pycache__, .idea, Pods, vendor, build, etc.
                    // Return WalkState::Skip: prevents both recording the entry AND descending.
                    // Guard depth > 0 so we never prune the walk root itself.
                    if is_dir && de.depth() > 0 && is_skip_dir(path) {
                        return WalkState::Skip;
                    }

                    // Privacy: prune credential/key stores (.ssh, .gnupg, Keychains, browser
                    // profiles) unless the caller explicitly opted in via `include_sensitive`.
                    // WalkState::Skip stops both recording the dir entry AND descending into it.
                    if is_dir && de.depth() > 0 && !include_sensitive && is_sensitive_dir(path) {
                        return WalkState::Skip;
                    }

                    // Apply the combined gitignore matcher: root .gitignore patterns +
                    // [scan] ignore config patterns. Both are anchored at root.
                    if let Some(m) = &combined_matcher {
                        if m.matched(path, is_dir).is_ignore() {
                            // Skip dirs (prunes subtree); for files just don't record — continue
                            // so the walker keeps processing siblings.
                            if is_dir {
                                return WalkState::Skip;
                            } else {
                                return WalkState::Continue; // don't push; move on
                            }
                        }
                    }

                    // Fetch metadata for size / mtime; skip entries we can't read.
                    let meta = match de.metadata() {
                        Ok(m) => m,
                        Err(_) => return WalkState::Continue,
                    };

                    let kind = if meta.is_dir() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    };

                    let hint = classify(path).or_else(|| {
                        if meta.is_file() {
                            classify_file_by_extension(path)
                        } else {
                            None
                        }
                    });

                    // Whole-computer groundwork: when opted in, NUL-sniff files so the deep phase
                    // can skip binaries without opening them. Fail-open (an unreadable file is
                    // never flagged). Only files; the entry itself is still recorded either way.
                    let is_binary = sniff_binary && meta.is_file() && file_is_binary(path);

                    entries.lock().unwrap().push(Entry {
                        path: path.to_path_buf(),
                        kind,
                        size: if meta.is_file() { meta.len() } else { 0 },
                        modified: meta.modified().ok(),
                        hint,
                        is_binary,
                    });
                    WalkState::Continue
                },
            )
        }
    });

    // `run()` is synchronous — all worker threads have finished by here.
    // The Arc clone moved into `run()` is dropped when `run()` returns, so
    // `Arc::try_unwrap` finds exactly one strong reference remaining (ours).
    Ok(Arc::try_unwrap(entries).unwrap().into_inner().unwrap())
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
        // node_modules/ and its contents are pruned by WalkState::Skip + is_skip_dir.
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
    fn prunes_vcs_cache_and_artifact_dirs() {
        // Nested `.gitignore` files are now honoured (nested per-dir, not just root),
        // AND build/VCS/cache directories are pruned by name via is_skip_dir so they
        // are excluded even without a .gitignore.
        let dir = tempfile::tempdir().unwrap();
        // Manifests that mark a recognized project, so the guarded build-dir rules
        // (Pods next to a Podfile, vendor next to go.mod, build next to a Makefile) fire.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        std::fs::write(dir.path().join("Podfile"), "platform :ios").unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x").unwrap();
        std::fs::write(dir.path().join("Makefile"), "all:").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let junk = [
            ".git",
            ".svn",
            ".hg",
            "node_modules",
            "target",
            "__pycache__",
            ".pytest_cache",
            ".mypy_cache",
            ".ruff_cache",
            ".tox",
            ".idea",
            ".dart_tool",
            "Pods",
            "vendor",
            "build",
        ];
        for j in junk {
            let d = dir.path().join(j).join("nested");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(dir.path().join(j).join("junk.txt"), "artifact").unwrap();
            std::fs::write(d.join("deep.txt"), "deep artifact").unwrap();
        }

        let entries = walk(dir.path(), &WalkConfig::default()).unwrap();
        let leaked: Vec<&str> = junk
            .iter()
            .copied()
            .filter(|j| {
                let needle = format!("/{j}/");
                entries
                    .iter()
                    .any(|e| e.path.to_string_lossy().contains(&needle))
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "these dirs leaked into the index: {leaked:?}"
        );
        assert!(
            entries.iter().any(|e| e.path.ends_with("main.rs")),
            "real source must still be indexed"
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
    fn sniff_binary_flags_nul_files_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();

        // Default (off): nothing is flagged — the walk stays metadata-only.
        let off = walk(dir.path(), &WalkConfig::default()).unwrap();
        assert!(
            off.iter().all(|e| !e.is_binary),
            "no file should be flagged when sniff_binary is off"
        );
        // The binary is still RECORDED (metadata preserved), just not flagged.
        assert!(off.iter().any(|e| e.path.ends_with("blob.bin")));

        // On: the NUL blob is flagged; the text file is not.
        let cfg = WalkConfig {
            sniff_binary: true,
            ..Default::default()
        };
        let on = walk(dir.path(), &cfg).unwrap();
        let bin = on.iter().find(|e| e.path.ends_with("blob.bin")).unwrap();
        let code = on.iter().find(|e| e.path.ends_with("code.rs")).unwrap();
        assert!(bin.is_binary, "NUL file must be flagged binary");
        assert!(!code.is_binary, "text file must not be flagged");
    }

    #[test]
    fn should_index_file_applies_scan_policy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let roots = vec![root.clone()];
        let matchers = build_scan_matchers(&roots, true, &["*.log".to_string()]);

        let write = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        };
        let cap = Some(DEFAULT_MAX_FILESIZE);

        // A normal source file is indexed.
        let ok = root.join("src/main.rs");
        write(&ok);
        assert!(should_index_file(&ok, &roots, false, cap, &matchers));

        // Build artifacts / VCS internals are skipped (name fast-path + classify path).
        let git = root.join(".git/objects/ab");
        write(&git);
        assert!(!should_index_file(&git, &roots, false, cap, &matchers));
        let nm = root.join("node_modules/pkg/index.js");
        write(&nm);
        assert!(!should_index_file(&nm, &roots, false, cap, &matchers));

        // A `[scan] ignore` / gitignore match is skipped.
        let log = root.join("app.log");
        write(&log);
        assert!(!should_index_file(&log, &roots, false, cap, &matchers));

        // Oversized files are skipped.
        assert!(!should_index_file(&ok, &roots, false, Some(0), &matchers));

        // Secret files (`.env`, `id_rsa`, `*.pem`) are metadata-only by default, but indexed
        // when the caller opts into sensitive content.
        let env = root.join("service/.env");
        write(&env);
        assert!(!should_index_file(&env, &roots, false, cap, &matchers));
        assert!(should_index_file(&env, &roots, true, cap, &matchers));
        let key = root.join("deploy/id_rsa");
        write(&key);
        assert!(!should_index_file(&key, &roots, false, cap, &matchers));
        // A Keynote deck (`.key`) is NOT a secret and stays indexable.
        let deck = root.join("talks/deck.key");
        write(&deck);
        assert!(should_index_file(&deck, &roots, false, cap, &matchers));
    }

    #[test]
    fn is_sensitive_file_flags_secrets_not_keynote() {
        assert!(is_sensitive_file(Path::new("/p/.env")));
        assert!(is_sensitive_file(Path::new("/p/.env.production")));
        assert!(is_sensitive_file(Path::new("/p/deploy.pem")));
        assert!(is_sensitive_file(Path::new("/p/id_ed25519")));
        assert!(!is_sensitive_file(Path::new("/p/deck.key")));
        assert!(!is_sensitive_file(Path::new("/p/src/main.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn walk_skips_symlinks_that_escape_the_root() {
        use std::os::unix::fs::symlink;
        // An out-of-root secret, reachable only via a link.
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real.rs"), "fn main() {}").unwrap();
        // Innocuously named so the name-based deny-list can't catch it — only is_symlink() can.
        symlink(&secret, root.path().join("notes.rs")).unwrap();

        let entries = walk(root.path(), &WalkConfig::default()).unwrap();
        assert!(
            entries.iter().any(|e| e.path.ends_with("real.rs")),
            "the real file must be indexed"
        );
        assert!(
            !entries.iter().any(|e| e.path.ends_with("notes.rs")),
            "the escaping symlink must NOT be indexed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_index_file_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("target.rs");
        std::fs::write(&target, "x").unwrap();
        let link = root.join("link.rs");
        symlink(&target, &link).unwrap();
        let roots = vec![root.clone()];
        let matchers = build_scan_matchers(&roots, false, &[]);
        let cap = Some(DEFAULT_MAX_FILESIZE);
        assert!(should_index_file(&target, &roots, false, cap, &matchers));
        assert!(!should_index_file(&link, &roots, false, cap, &matchers));
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

    #[test]
    fn max_filesize_skips_large_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.txt"), "tiny").unwrap();
        // Write a file just above the 5-byte cap.
        std::fs::write(dir.path().join("big.bin"), "123456").unwrap();

        let cfg = WalkConfig {
            max_filesize: Some(5),
            ..Default::default()
        };
        let entries = walk(dir.path(), &cfg).unwrap();
        assert!(entries.iter().any(|e| e.path.ends_with("small.txt")));
        assert!(
            !entries.iter().any(|e| e.path.ends_with("big.bin")),
            "files above max_filesize must be skipped"
        );
    }

    #[test]
    fn is_sensitive_dir_recognizes_credential_stores() {
        use std::path::PathBuf;
        // Classified DeepScanPolicy::Sensitive by surface::classify (path-contains predicates,
        // independent of the real home dir) — pruned from the walk unless include_sensitive.
        assert!(is_sensitive_dir(&PathBuf::from(
            "/Users/x/Library/Keychains"
        )));
        assert!(is_sensitive_dir(&PathBuf::from(
            "/Users/x/Library/Application Support/Google/Chrome"
        )));
        assert!(is_sensitive_dir(&PathBuf::from(
            "/Users/x/Library/Application Support/Firefox"
        )));
        // A normal code dir is not sensitive.
        assert!(!is_sensitive_dir(&PathBuf::from("/Users/x/projects/myapp")));
    }
}
