//! Self-update: check for a newer Indexa release on GitHub and atomically
//! replace the running binary with the downloaded one.
//!
//! All requests use the **public** GitHub API/CDN — no authentication is needed
//! once `harf-promo/indexa` is public. The rustls TLS stack is used throughout;
//! OpenSSL is never linked.

use std::io::Write as _;

use anyhow::Context as _;
use reqwest::Client;
use semver::Version;
use serde::Deserialize;

const REPO: &str = "harf-promo/indexa";
const USER_AGENT: &str = concat!("indexa/", env!("CARGO_PKG_VERSION"));

/// Information returned by [`check`].
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    /// Semver string of the running binary, e.g. `"0.11.0"`.
    pub current: String,
    /// Semver string of the latest GitHub Release, e.g. `"0.12.0"`.
    pub latest: String,
    /// Raw tag used in release download URLs, e.g. `"v0.12.0"`.
    pub latest_tag: String,
    /// `true` when `latest` > `current` per semver ordering.
    pub update_available: bool,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

fn build_client() -> anyhow::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build HTTP client")
}

/// Returns the literal release asset filename for the running platform.
///
/// Asset names are mapped explicitly (not by triple substring) to match the
/// naming used in `.github/workflows/release.yml`.
fn asset_name() -> anyhow::Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "indexa-aarch64-apple-darwin",
        ("macos", "x86_64") => "indexa-x86_64-apple-darwin",
        ("linux", "x86_64") => "indexa-x86_64-linux-gnu",
        ("linux", "aarch64") => "indexa-aarch64-linux-gnu",
        ("windows", "x86_64") => "indexa-x86_64-windows.exe",
        (os, arch) => anyhow::bail!(
            "no prebuilt binary for {os}/{arch} — \
                 build from source: cargo build --release -p indexa"
        ),
    })
}

/// Query the GitHub Releases API for the latest published release.
///
/// Does not require authentication (public repo). The GitHub API requires a
/// `User-Agent` header; [`USER_AGENT`] provides it.
///
/// Note: `/releases/latest` excludes drafts and pre-releases.
pub async fn check() -> anyhow::Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = build_client()?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("GitHub API request failed")?;

    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }

    let rel: GhRelease = resp
        .json()
        .await
        .context("unexpected GitHub API response shape")?;

    let latest_tag = rel.tag_name;
    let latest = latest_tag.trim_start_matches('v').to_string();
    let current = env!("CARGO_PKG_VERSION").to_string();

    let update_available = match (Version::parse(&latest), Version::parse(&current)) {
        (Ok(l), Ok(c)) => l > c,
        _ => {
            tracing::warn!(
                current = %current, latest = %latest,
                "could not compare versions as semver; assuming no update"
            );
            false
        }
    };

    Ok(ReleaseInfo {
        current,
        latest,
        latest_tag,
        update_available,
    })
}

/// True when `exe` lives inside a macOS `.app` bundle (`…/Foo.app/Contents/MacOS/bin`).
///
/// The binary self-replace [`apply`] performs is for the standalone CLI only.
/// Inside a `.app`, replacing the Mach-O downloads the wrong artifact (the
/// headless CLI binary, not the GUI app), leaves the bundle's `Info.plist` and
/// resources stale, and ad-hoc re-signing strips the Developer-ID + notarization
/// — bricking the app. The desktop must update through its own (Tauri) updater.
fn is_inside_app_bundle(exe: &std::path::Path) -> bool {
    use std::path::Component;
    exe.components().collect::<Vec<_>>().windows(3).any(|w| {
        matches!(w[0], Component::Normal(s) if s.to_string_lossy().ends_with(".app"))
            && matches!(w[1], Component::Normal(s) if s == "Contents")
            && matches!(w[2], Component::Normal(s) if s == "MacOS")
    })
}

/// The reason a binary self-replace must be refused, or `None` when it is safe.
/// Pure (no env/fs access) so the guard is unit-tested directly; [`apply`] feeds
/// it the live `current_exe()` and `INDEXA_DESKTOP` state.
fn self_replace_refusal(exe: Option<&std::path::Path>, is_desktop: bool) -> Option<String> {
    if is_desktop {
        return Some(
            "self-update is disabled inside the Indexa desktop app — use the menu-bar \
             \"Check for Updates…\" to update the app instead"
                .to_string(),
        );
    }
    match exe {
        Some(p) if is_inside_app_bundle(p) => Some(format!(
            "refusing to self-replace a binary inside a macOS .app bundle ({}) — \
             this would corrupt the bundle; update the app through its own updater",
            p.display()
        )),
        _ => None,
    }
}

/// Download the release asset for `tag` and atomically replace the running
/// binary. Returns the semver version string that was installed (without
/// leading `v`), e.g. `"0.12.1"`.
///
/// `tag` may be `"v0.12.1"` or `"0.12.1"` — a leading `v` is added if
/// needed for the download URL.
///
/// Refuses to run inside the Indexa desktop app (binary self-replace would
/// corrupt the `.app` bundle — see [`is_inside_app_bundle`]); the desktop
/// updates via its built-in updater.
///
/// # Errors
///
/// Returns a human-readable, actionable error on:
/// - Running inside a `.app` bundle / the desktop app.
/// - Permission denied (binary in root-owned dir like `/usr/local/bin`).
/// - Truncated or empty download.
/// - Non-existent release/asset (404).
pub async fn apply(tag: &str) -> anyhow::Result<String> {
    // Guard (defense in depth): never self-replace the desktop app's bundled
    // Mach-O. Both signals are checked because either alone is sufficient and
    // they fail independently — INDEXA_DESKTOP is set by the desktop process,
    // and the path shape catches any other way the desktop binary could invoke
    // this (e.g. a future caller that doesn't set the env var).
    if let Some(reason) = self_replace_refusal(
        std::env::current_exe().ok().as_deref(),
        std::env::var("INDEXA_DESKTOP").as_deref() == Ok("1"),
    ) {
        anyhow::bail!(reason);
    }

    let tag_str = if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    };
    let version_str = tag.trim_start_matches('v').to_string();

    let asset = asset_name()?;
    let url = format!("https://github.com/{REPO}/releases/download/{tag_str}/{asset}");

    tracing::info!(%url, "downloading update");

    let client = build_client()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .context("download request failed")?;

    if !resp.status().is_success() {
        anyhow::bail!(
            "download failed (HTTP {}): tag={tag_str} asset={asset}\n\
             Make sure the release exists at https://github.com/{REPO}/releases",
            resp.status()
        );
    }

    let content_len = resp.content_length();
    let bytes = resp.bytes().await.context("download stream interrupted")?;

    if bytes.is_empty() {
        anyhow::bail!("downloaded binary is empty — the release asset may be missing");
    }
    if let Some(expected) = content_len {
        let got = bytes.len() as u64;
        if got != expected {
            anyhow::bail!(
                "download truncated: received {got} bytes, expected {expected} — aborting to protect the running binary"
            );
        }
    }

    // Determine where the running exe lives — the temp file must be on the
    // same filesystem for `self_replace` to do an atomic rename.
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("cannot determine path to the running executable")?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("running executable has no parent directory"))?
        .to_path_buf();

    // All file I/O (including self_replace) is blocking; run on the thread pool.
    #[cfg(target_os = "macos")]
    let exe_clone = exe.clone(); // for the post-replace re-sign step on macOS
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut tmp = tempfile::Builder::new()
            .prefix(".indexa-update-")
            .tempfile_in(&exe_dir)
            .map_err(|e| permission_error(e, &exe_dir))?;

        tmp.write_all(&bytes).context("write to temp file failed")?;
        tmp.flush().context("flush temp file failed")?;

        // Ensure the new binary is executable on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o755))
                .context("chmod on temp file failed")?;
        }

        // Persist the temp file so self_replace can rename it into place;
        // auto-deletion on drop would remove it before the rename.
        let (_, tmp_path) = tmp
            .keep()
            .map_err(|e| anyhow::anyhow!("could not persist temp file: {e}"))?;

        // Atomically replace the running binary. On UNIX: rename(2). On Windows:
        // MoveFileExW with MOVEFILE_REPLACE_EXISTING (old exe is deleted at exit).
        self_replace::self_replace(&tmp_path).map_err(|e| permission_error(e, &tmp_path))?;

        // macOS 26+ Code Signing Monitor invalidates the trust record when a
        // binary at a known path is overwritten, even with an identical ad-hoc
        // signature. Re-signing forces a fresh evaluation so the new binary
        // actually runs. The `codesign` tool ships with Xcode Command Line
        // Tools; we warn (but don't abort) if it is absent or returns non-zero,
        // because a missing re-sign means the binary will fail to launch on
        // macOS 26+ — a user-visible failure that was previously silently swallowed.
        #[cfg(target_os = "macos")]
        if let Some(path_str) = exe_clone.to_str() {
            match std::process::Command::new("codesign")
                .args(["--force", "--sign", "-", path_str])
                .output()
            {
                Ok(out) if out.status.success() => {
                    tracing::debug!("codesign re-sign succeeded for {path_str}");
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    tracing::warn!(
                        path = path_str,
                        exit_code = ?out.status.code(),
                        stderr = %stderr.trim(),
                        "codesign re-sign failed after update; \
                         the new binary may not launch on macOS 26+ \
                         (Code Signing Monitor). \
                         Run: codesign --force --sign - {}",
                        path_str
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = path_str,
                        error = %e,
                        "could not run `codesign` after update; \
                         install Xcode Command Line Tools if you see \
                         a 'killed' error on next launch."
                    );
                }
            }
        }

        Ok(())
    })
    .await
    .context("update task panicked")??;

    tracing::info!(version = %version_str, "update applied — restart to run the new version");
    Ok(version_str)
}

/// Download the matching CLI binary for this platform from release `tag` into `dir`, writing it
/// as `indexa` (`indexa.exe` on Windows), chmod 0755 + ad-hoc-codesign on macOS. Returns the
/// installed path.
///
/// Unlike [`apply`], this writes to a *target directory* and never self-replaces — so the desktop
/// app can install or refresh the user's standalone CLI (the desktop has no CLI of its own, and
/// the self-replace guard intentionally blocks updating inside the `.app`). The integrity checks
/// mirror `apply`.
///
/// `on_progress(downloaded_bytes, total_bytes)` is called after each received chunk so a caller
/// (the desktop app) can render a live progress bar; `total_bytes` is `None` when the server omits
/// `Content-Length`. The body is read chunk-by-chunk via `Response::chunk` (no `reqwest` `stream`
/// feature needed) rather than `.bytes()` all-at-once, so progress is real.
pub async fn download_cli_to(
    dir: &std::path::Path,
    tag: &str,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> anyhow::Result<std::path::PathBuf> {
    let tag_str = if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    };
    let asset = asset_name()?;
    let url = format!("https://github.com/{REPO}/releases/download/{tag_str}/{asset}");
    tracing::info!(%url, "downloading CLI binary");

    let client = build_client()?;
    let mut resp = client
        .get(&url)
        .send()
        .await
        .context("download request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "download failed (HTTP {}): tag={tag_str} asset={asset}",
            resp.status()
        );
    }
    let content_len = resp.content_length();
    // Stream the body so we can report progress per chunk. `chunk()` is available without the
    // `stream` cargo feature, unlike `bytes_stream()`.
    let mut bytes: Vec<u8> = Vec::with_capacity(content_len.unwrap_or(0) as usize);
    while let Some(chunk) = resp.chunk().await.context("download stream interrupted")? {
        bytes.extend_from_slice(&chunk);
        if let Some(cb) = on_progress {
            cb(bytes.len() as u64, content_len);
        }
    }
    if bytes.is_empty() {
        anyhow::bail!("downloaded binary is empty — the release asset may be missing");
    }
    if let Some(expected) = content_len {
        if bytes.len() as u64 != expected {
            anyhow::bail!("download truncated — aborting");
        }
    }

    let bin_name = if cfg!(windows) {
        "indexa.exe"
    } else {
        "indexa"
    };
    let dir = dir.to_path_buf();
    let dest = dir.join(bin_name);
    let dest_for_task = dest.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir).map_err(|e| permission_error(e, &dir))?;
        // Write to a temp file in the target dir, then rename into place (atomic on the same fs).
        let mut tmp = tempfile::Builder::new()
            .prefix(".indexa-cli-")
            .tempfile_in(&dir)
            .map_err(|e| permission_error(e, &dir))?;
        tmp.write_all(&bytes).context("write to temp file failed")?;
        tmp.flush().context("flush temp file failed")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o755))
                .context("chmod on temp file failed")?;
        }
        let (_, tmp_path) = tmp
            .keep()
            .map_err(|e| anyhow::anyhow!("could not persist temp file: {e}"))?;
        std::fs::rename(&tmp_path, &dest_for_task)
            .map_err(|e| permission_error(e, &dest_for_task))?;
        // macOS: ad-hoc sign so Gatekeeper lets the freshly-written binary run (best-effort).
        #[cfg(target_os = "macos")]
        if let Some(p) = dest_for_task.to_str() {
            let _ = std::process::Command::new("codesign")
                .args(["--force", "--sign", "-", p])
                .output();
        }
        Ok(())
    })
    .await
    .context("CLI install task panicked")??;

    tracing::info!(path = %dest.display(), "CLI installed");
    Ok(dest)
}

/// Build a human-readable, actionable error for a file-write permission failure.
fn permission_error(source: impl std::fmt::Display, path: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot write to {path}: {source}\n\
        \n\
        The binary is likely in a root-owned directory (e.g. /usr/local/bin).\n\
        Try one of:\n\
          • sudo indexa update\n\
          • Re-download from https://github.com/{REPO}/releases/latest and \
            replace the binary manually",
        path = path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::{is_inside_app_bundle, self_replace_refusal};
    use std::path::Path;

    #[test]
    fn detects_macos_app_bundle_binaries() {
        assert!(is_inside_app_bundle(Path::new(
            "/Applications/Indexa.app/Contents/MacOS/indexa-desktop"
        )));
        // Nested .app, and a non-standard install location, still match.
        assert!(is_inside_app_bundle(Path::new(
            "/Users/x/Applications/Indexa.app/Contents/MacOS/indexa-desktop"
        )));
    }

    #[test]
    fn plain_cli_binaries_are_not_app_bundles() {
        for p in [
            "/usr/local/bin/indexa",
            "/Users/x/.cargo/bin/indexa",
            "/opt/homebrew/bin/indexa",
            // A directory merely named with .app somewhere but not the bundle shape.
            "/Users/x/my.app-notes/indexa",
            "/tmp/Contents/MacOS/indexa", // no *.app ancestor
        ] {
            assert!(!is_inside_app_bundle(Path::new(p)), "false positive on {p}");
        }
    }

    #[test]
    fn refuses_self_replace_in_desktop_or_bundle() {
        // Desktop env flag alone refuses, regardless of path.
        assert!(self_replace_refusal(Some(Path::new("/usr/local/bin/indexa")), true).is_some());
        // .app bundle path refuses even without the env flag.
        let r = self_replace_refusal(
            Some(Path::new(
                "/Applications/Indexa.app/Contents/MacOS/indexa-desktop",
            )),
            false,
        );
        assert!(r.unwrap().contains(".app bundle"));
        // A plain CLI binary, not desktop → allowed (no refusal).
        assert!(self_replace_refusal(Some(Path::new("/usr/local/bin/indexa")), false).is_none());
        // Unknown exe path, not desktop → allowed (we don't block what we can't classify).
        assert!(self_replace_refusal(None, false).is_none());
    }
}
