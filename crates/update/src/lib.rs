//! Self-update: check for a newer Indexa release on GitHub and atomically
//! replace the running binary with the downloaded one.
//!
//! All requests use the **public** GitHub API/CDN — no authentication is needed
//! because `harf-promo/indexa` is public. The rustls TLS stack is used throughout;
//! OpenSSL is never linked.

use std::io::Write as _;

use anyhow::Context as _;
use reqwest::Client;
use semver::Version;
use serde::Deserialize;

mod skew;
pub use skew::{
    classify_skew, detect_skew, installed_app_version, Skew, Surface, CLI_SKEW_MARKER_FILE,
};

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
        // A stalled release host (no FIN, no bytes) must not hang the update forever. Generous
        // whole-request cap (binaries are tens of MB) + a short connect timeout.
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")
}

/// Minisign public key (key ID `4A0852406D06E275`) — the SAME key the desktop's Tauri updater
/// verifies the app bundle with (base64-decoded from `apps/indexa-desktop/tauri.conf.json`). CLI
/// release assets are signed with its private half in `.github/workflows/release.yml`.
const MINISIGN_PUBKEY_B64: &str = "RWR14gZtQFIISnysTnP1hTZ1o/OHzJenqE1f0SpTNe0W/UjFr5yfR1Uv";

/// Fetch `{asset_url}.sig` and verify `bytes` against [`MINISIGN_PUBKEY_B64`].
///
/// **Fail-open when no signature is published** (HTTP error / empty): pre-signature release tags
/// have no `.sig`, and refusing them would break self-update for everyone until the next signed
/// release. A signature that IS published but does NOT verify is a hard error (tampering). The
/// `.sig` is the Tauri format — base64 of a standard minisign signature file — so we base64-decode
/// it before parsing, matching how the desktop verifies the bundle.
async fn verify_asset_signature(
    client: &Client,
    asset_url: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let sig_url = format!("{asset_url}.sig");
    let sig_b64 = match client.get(&sig_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(t) if !t.trim().is_empty() => t,
            _ => {
                tracing::warn!(%sig_url, "signature asset is empty — installing UNVERIFIED (pre-signature release)");
                return Ok(());
            }
        },
        _ => {
            tracing::warn!(%sig_url, "no signature published for this release — installing UNVERIFIED (pre-signature release)");
            return Ok(());
        }
    };

    verify_minisign(MINISIGN_PUBKEY_B64, &sig_b64, bytes)?;
    tracing::info!("update signature verified (minisign 4A0852406D06E275)");
    Ok(())
}

/// Verify `bytes` against a base64-wrapped minisign signature file (`sig_b64` — the Tauri `.sig`
/// format) using `pubkey_b64`. Pure (no I/O) so it is unit-tested with a real Tauri-format vector;
/// [`verify_asset_signature`] fetches `sig_b64` and calls this.
fn verify_minisign(pubkey_b64: &str, sig_b64: &str, bytes: &[u8]) -> anyhow::Result<()> {
    use base64::Engine;
    let sig_file = base64::engine::general_purpose::STANDARD
        .decode(sig_b64.trim())
        .context("update signature is not valid base64")?;
    let sig_text = std::str::from_utf8(&sig_file).context("update signature is not valid UTF-8")?;
    let signature = minisign_verify::Signature::decode(sig_text)
        .map_err(|e| anyhow::anyhow!("malformed update signature: {e}"))?;
    let pubkey = minisign_verify::PublicKey::from_base64(pubkey_b64)
        .map_err(|e| anyhow::anyhow!("minisign public key is invalid: {e}"))?;
    pubkey.verify(bytes, &signature, false).map_err(|e| {
        anyhow::anyhow!(
            "UPDATE SIGNATURE VERIFICATION FAILED — refusing to install (the download may be \
             tampered or corrupt): {e}"
        )
    })
}

/// Cheap sanity check that `bytes` is an executable for some platform (Mach-O / ELF / PE) — so an
/// HTML error page, a truncated download, or an LFS pointer can't be self-replaced over the running
/// binary. Defense-in-depth beside the signature (a valid signature already implies authenticity;
/// this just yields a clearer error for an obviously-wrong payload).
fn looks_like_executable(bytes: &[u8]) -> bool {
    let head4 = bytes.get(0..4);
    matches!(
        head4,
        Some(b"\x7fELF")                       // ELF (Linux)
            | Some([0xFE, 0xED, 0xFA, 0xCE])   // Mach-O 32-bit
            | Some([0xFE, 0xED, 0xFA, 0xCF])   // Mach-O 64-bit
            | Some([0xCE, 0xFA, 0xED, 0xFE])   // Mach-O 32-bit (byte-swapped)
            | Some([0xCF, 0xFA, 0xED, 0xFE])   // Mach-O 64-bit (byte-swapped)
            | Some([0xCA, 0xFE, 0xBA, 0xBE])   // Mach-O universal (fat)
            | Some([0xBE, 0xBA, 0xFE, 0xCA]) // Mach-O universal (byte-swapped)
    ) || bytes.starts_with(b"MZ") // PE (Windows)
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

/// Parse the semver version from a CHANGELOG section header line, e.g.
/// `## [0.51.0] — 2026-06-16` → `0.51.0`. Anchors on the **bracketed version only**;
/// the date separator in this CHANGELOG is an em-dash (U+2014), so never split on
/// ` - `. Returns `None` for non-version headers like `## [Unreleased]`.
fn section_version(line: &str) -> Option<Version> {
    let after_hashes = line.trim_start().strip_prefix("##")?.trim_start();
    let inner = after_hashes.strip_prefix('[')?;
    let end = inner.find(']')?;
    Version::parse(inner[..end].trim()).ok()
}

/// Assemble the CHANGELOG sections a user gains by updating `from` → `to`: every
/// version section `V` with `from < V <= to`, in the file's natural newest-first
/// order. The `## [Unreleased]` section and any non-semver header are skipped.
///
/// Returns an empty string when nothing qualifies (same version, a downgrade, or a
/// parse miss) so the caller can fall back to the single newest section.
pub fn cumulative_changelog(full_md: &str, from: &Version, to: &Version) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in full_md.lines() {
        // A new top-level section ("## …") decides what we keep next. Sub-headings
        // ("### …") begin with "###" and so never match "## " — they ride along with
        // their parent section's keep state, as does the section body.
        if line.starts_with("## ") {
            keep = match section_version(line) {
                Some(v) => v > *from && v <= *to,
                None => false,
            };
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Fetch the tag-pinned CHANGELOG and assemble the cumulative release notes for a
/// `from` → `to` update (see [`cumulative_changelog`]).
///
/// `latest.json` (what the updater surfaces as `Update.body`) carries only the single
/// newest section, because it is baked at release time and cannot know which version
/// the user is coming from. Only the client knows both ends, so the span is assembled
/// here: the CHANGELOG is read from `raw.githubusercontent.com` at tag `v{to}` — the
/// immutable copy shipped with the release being installed, so it always contains every
/// section up to `to`. Public repo, so no auth (see module docs); reuses the rustls
/// client and never links OpenSSL.
///
/// Fails open: any error (offline, 404, unparseable versions) is returned so the caller
/// falls back to the single newest section. A changelog hiccup must never block an update.
pub async fn cumulative_notes(from: &str, to: &str) -> anyhow::Result<String> {
    let from_v =
        Version::parse(from.trim_start_matches('v')).context("installed version is not semver")?;
    let to_v =
        Version::parse(to.trim_start_matches('v')).context("target version is not semver")?;
    if from_v >= to_v {
        // Same version or a downgrade — nothing gained; let the caller use the single section.
        return Ok(String::new());
    }
    let url = format!("https://raw.githubusercontent.com/{REPO}/v{to_v}/CHANGELOG.md");
    let client = build_client()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .context("CHANGELOG fetch failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("CHANGELOG fetch returned {}", resp.status());
    }
    let md = resp.text().await.context("CHANGELOG body read failed")?;
    Ok(cumulative_changelog(&md, &from_v, &to_v))
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

    // Cryptographically verify the download before replacing the running binary (fail-open only for
    // pre-signature releases), and sanity-check the magic bytes.
    verify_asset_signature(&client, &url, &bytes).await?;
    if !looks_like_executable(&bytes) {
        anyhow::bail!(
            "downloaded asset is not a recognized executable (Mach-O/ELF/PE) — refusing to install"
        );
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

    // Verify the signature (fail-open only for pre-signature releases) + magic bytes before this
    // binary is written to a PATH dir and later executed as `indexa`.
    verify_asset_signature(&client, &url, &bytes).await?;
    if !looks_like_executable(&bytes) {
        anyhow::bail!(
            "downloaded asset is not a recognized executable (Mach-O/ELF/PE) — refusing to install"
        );
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
        // macOS: ad-hoc sign so Gatekeeper (and the macOS 26+ Code Signing Monitor)
        // lets the freshly-written binary run. Best-effort, but NOT silent — a failed
        // sign means the next `indexa` launch is killed (exit 137), so we surface it
        // the same way `apply` does instead of swallowing the error.
        #[cfg(target_os = "macos")]
        if let Some(p) = dest_for_task.to_str() {
            match std::process::Command::new("codesign")
                .args(["--force", "--sign", "-", p])
                .output()
            {
                Ok(out) if out.status.success() => {
                    tracing::debug!("codesign ad-hoc sign succeeded for {p}");
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    tracing::warn!(
                        path = p,
                        exit_code = ?out.status.code(),
                        stderr = %stderr.trim(),
                        "codesign failed on the freshly-installed CLI; \
                         it may be killed on launch on macOS 26+. \
                         Run: codesign --force --sign - {}",
                        p
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = p,
                        error = %e,
                        "could not run `codesign` on the installed CLI; \
                         install Xcode Command Line Tools if you see a 'killed' error."
                    );
                }
            }
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
    use super::{
        cumulative_changelog, is_inside_app_bundle, looks_like_executable, self_replace_refusal,
        verify_minisign, MINISIGN_PUBKEY_B64,
    };
    use semver::Version;
    use std::path::Path;

    // Real Tauri-format test vector — generated with `tauri signer generate` + `tauri signer sign`
    // over a TEST key (NOT the release key). A public key + a signature are safe to commit; this
    // proves `verify_minisign` handles the exact `.sig` format `release.yml` produces.
    const TEST_PUBKEY: &str = "RWSw/VA8WGxtADk+aLoZA7hZWsGqysn5SWCvU2eoLfwEoelvw8ydG1aM";
    const TEST_MSG: &[u8] = b"indexa-update-signature-test-payload";
    const TEST_SIG_B64: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTdy9WQThXR3h0QU8xYmorRXUralNSdHdBRitoc3dZNHB2Z2hhaU1YQ0p3TlpUOFp2M3B3Y2RoUWFURUtLTjg4MElubmtBdGZ4NlpIckgyYmRYUWpTRkd1eEJmOGZGVUE0PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzgzMzY3MDg3CWZpbGU6YmxvYgpWWHV1aXU5NVVUOUExUUFISkhpYkRaL0tZT2VVRHdXVVlPNHdMT0Z6MncyVER4Ykp3c01IUkNZRUdja2d6M240K0FWQnZiWThYd3NjZ20vRDBxTy9EZz09Cg==";

    #[test]
    fn verify_minisign_accepts_valid_rejects_tampered_and_wrong_key() {
        // Correct message + key + signature verifies.
        assert!(verify_minisign(TEST_PUBKEY, TEST_SIG_B64, TEST_MSG).is_ok());
        // Tampered payload fails (this is the anti-tamper guarantee).
        assert!(verify_minisign(TEST_PUBKEY, TEST_SIG_B64, b"tampered payload").is_err());
        // A signature made by a different key does not verify against the real release pubkey.
        assert!(verify_minisign(MINISIGN_PUBKEY_B64, TEST_SIG_B64, TEST_MSG).is_err());
        // Garbage signature is rejected, not panicked on.
        assert!(verify_minisign(TEST_PUBKEY, "not-base64!!", TEST_MSG).is_err());
    }

    #[test]
    fn looks_like_executable_accepts_binaries_rejects_html() {
        assert!(looks_like_executable(b"\x7fELF\x02\x01\x01\x00")); // ELF
        assert!(looks_like_executable(&[0xCF, 0xFA, 0xED, 0xFE, 0, 0])); // Mach-O 64
        assert!(looks_like_executable(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0])); // Mach-O fat
        assert!(looks_like_executable(b"MZ\x90\x00")); // PE
        assert!(!looks_like_executable(b"<!DOCTYPE html><html>404")); // error page
        assert!(!looks_like_executable(b"version https://git-lfs")); // LFS pointer
        assert!(!looks_like_executable(b"")); // empty
    }

    // A miniature CHANGELOG mirroring the real format: em-dash date separator,
    // an `## [Unreleased]` section, a `# Changelog` preamble, and `### Added` sub-headings.
    const SAMPLE: &str = "\
# Changelog

All notable changes to this project.

## [Unreleased]

- nothing yet

## [0.51.0] — 2026-06-16

### Added
- ui polish

## [0.50.0] — 2026-06-16

### Added
- format wave 3

## [0.49.0] — 2026-06-16

### Added
- formats list

## [0.48.0] — 2026-06-16

### Added
- email parser
";

    #[test]
    fn cumulative_changelog_collects_only_the_gained_versions() {
        let from = Version::parse("0.48.0").unwrap();
        let to = Version::parse("0.51.0").unwrap();
        let out = cumulative_changelog(SAMPLE, &from, &to);
        // Gains 0.51 / 0.50 / 0.49 — NOT the installed 0.48, NOT Unreleased, NOT the preamble.
        assert!(out.contains("## [0.51.0]"));
        assert!(out.contains("## [0.50.0]"));
        assert!(out.contains("## [0.49.0]"));
        assert!(!out.contains("## [0.48.0]"));
        assert!(!out.contains("Unreleased"));
        assert!(!out.contains("All notable changes"));
        // Section bodies ride along; the installed version's body does not leak in.
        assert!(out.contains("ui polish"));
        assert!(out.contains("formats list"));
        assert!(!out.contains("email parser"));
        // Newest-first order is preserved (0.51 precedes 0.49).
        assert!(out.find("0.51.0").unwrap() < out.find("0.49.0").unwrap());
    }

    #[test]
    fn cumulative_changelog_is_empty_when_nothing_gained() {
        let v51 = Version::parse("0.51.0").unwrap();
        let v50 = Version::parse("0.50.0").unwrap();
        // Same version → no gain.
        assert_eq!(cumulative_changelog(SAMPLE, &v51, &v51), "");
        // Downgrade (from newer than to) → no gain.
        assert_eq!(cumulative_changelog(SAMPLE, &v51, &v50), "");
    }

    #[test]
    fn cumulative_changelog_includes_the_target_section() {
        // Single-step update gains exactly the target section.
        let from = Version::parse("0.50.0").unwrap();
        let to = Version::parse("0.51.0").unwrap();
        let out = cumulative_changelog(SAMPLE, &from, &to);
        assert!(out.contains("## [0.51.0]"));
        assert!(out.contains("ui polish"));
        assert!(!out.contains("## [0.50.0]"));
    }

    #[test]
    fn cumulative_changelog_skips_non_semver_headers() {
        let md = "## [Unreleased]\n- x\n## [not-a-version]\n- y\n## [0.51.0] — z\n- real\n";
        let from = Version::parse("0.50.0").unwrap();
        let to = Version::parse("0.51.0").unwrap();
        let out = cumulative_changelog(md, &from, &to);
        assert!(out.contains("## [0.51.0]"));
        assert!(out.contains("real"));
        assert!(!out.contains("Unreleased"));
        assert!(!out.contains("not-a-version"));
    }

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
