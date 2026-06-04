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

/// Download the release asset for `tag` and atomically replace the running
/// binary. Returns the semver version string that was installed (without
/// leading `v`), e.g. `"0.12.1"`.
///
/// `tag` may be `"v0.12.1"` or `"0.12.1"` — a leading `v` is added if
/// needed for the download URL.
///
/// # Errors
///
/// Returns a human-readable, actionable error on:
/// - Permission denied (binary in root-owned dir like `/usr/local/bin`).
/// - Truncated or empty download.
/// - Non-existent release/asset (404).
pub async fn apply(tag: &str) -> anyhow::Result<String> {
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
