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

/// The first release whose CLI assets all carry a minisign `.sig`. Every release since has one and
/// none before it does (checked against the published release assets, v0.77.0–v0.80.3). From this
/// version on, a signature that is missing, unreachable or empty aborts the update: anyone able to
/// alter release assets could otherwise just delete the `.sig` to get an unverified binary installed.
const FIRST_SIGNED_VERSION: Version = Version::new(0, 77, 0);

/// Upper bound on a downloaded CLI asset (current ones are 50–80 MB), so a missing or lying
/// `Content-Length` can't grow memory without bound.
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound on a downloaded `.sig` (they are ~400 bytes).
const MAX_SIG_BYTES: u64 = 64 * 1024;

/// Set to `1` to let [`apply`] install a release older than the running binary. Off by default: an
/// older, correctly signed release still verifies, so a downgrade must be an explicit choice.
pub const ALLOW_DOWNGRADE_ENV: &str = "INDEXA_UPDATE_ALLOW_DOWNGRADE";

/// Parse a release tag (`v0.81.0` or `0.81.0`) as strict semver and return it with the canonical
/// `v`-prefixed tag rebuilt from the parsed version. The tag is interpolated into download URLs, so
/// anything that isn't exactly a version (`v1/../../other/repo/…`, `v1?x`, `%2e`) is rejected here
/// instead of being normalised by the URL parser into some other repository's asset.
fn parse_release_tag(tag: &str) -> anyhow::Result<(Version, String)> {
    let raw = tag.strip_prefix('v').unwrap_or(tag);
    let version = Version::parse(raw).with_context(|| {
        format!("invalid release tag {tag:?} — expected a version like v0.81.0")
    })?;
    let canonical = format!("v{version}");
    Ok((version, canonical))
}

/// The reason [`apply`] must refuse to install `target` over `current`, or `None` when it may.
/// Pure so the policy is unit-tested; `apply` feeds it [`ALLOW_DOWNGRADE_ENV`].
fn downgrade_refusal(current: &Version, target: &Version, allow_downgrade: bool) -> Option<String> {
    (target < current && !allow_downgrade).then(|| {
        format!(
            "refusing to downgrade v{current} → v{target}: an older release can carry bugs or \
             vulnerabilities that later releases fixed. Set {ALLOW_DOWNGRADE_ENV}=1 to install it \
             anyway."
        )
    })
}

/// What fetching `{asset}.sig` produced, kept apart from the HTTP client so the policy in
/// [`signature_to_verify`] is unit-testable.
#[derive(Debug)]
enum SigFetch {
    /// A 2xx response with this body.
    Body(String),
    /// A non-2xx response.
    Status(reqwest::StatusCode),
    /// No usable response: connect/timeout/read error.
    Failed(String),
}

/// Decide what to do with a fetched signature for release `version`: `Ok(Some(sig))` means
/// verify it, `Ok(None)` means a pre-signature release with no `.sig` (installed unverified), and
/// `Err` aborts the update. Fails closed: only an explicit 404 for a release older than
/// [`FIRST_SIGNED_VERSION`] skips verification. An error page, a timeout or an empty file is never
/// taken to mean "this release is unsigned".
fn signature_to_verify(version: &Version, fetch: SigFetch) -> anyhow::Result<Option<String>> {
    let signed_release = *version >= FIRST_SIGNED_VERSION;
    match fetch {
        SigFetch::Body(sig) if !sig.trim().is_empty() => Ok(Some(sig)),
        SigFetch::Body(_) => anyhow::bail!(
            "the update signature for v{version} is empty — refusing to install an unverified binary"
        ),
        SigFetch::Status(status) if status == reqwest::StatusCode::NOT_FOUND && !signed_release => {
            Ok(None)
        }
        SigFetch::Status(status) if status == reqwest::StatusCode::NOT_FOUND => anyhow::bail!(
            "no update signature is published for v{version}, but every release since \
             v{FIRST_SIGNED_VERSION} is signed — refusing to install an unverified binary"
        ),
        SigFetch::Status(status) => anyhow::bail!(
            "could not fetch the update signature for v{version} (HTTP {status}) — refusing to \
             install an unverified binary; try again later"
        ),
        SigFetch::Failed(e) => anyhow::bail!(
            "could not fetch the update signature for v{version} ({e}) — refusing to install an \
             unverified binary; try again later"
        ),
    }
}

/// Fetch `{asset_url}.sig` and verify `bytes` against `pubkey_b64` ([`MINISIGN_PUBKEY_B64`] in
/// production), failing closed per [`signature_to_verify`]. A signature that is published but does
/// not verify is always a hard error (tampering). The `.sig` is the Tauri format — base64 of a
/// standard minisign signature file — so we base64-decode it before parsing, matching how the
/// desktop verifies the bundle.
async fn verify_asset_signature(
    client: &Client,
    asset_url: &str,
    version: &Version,
    bytes: &[u8],
    pubkey_b64: &str,
) -> anyhow::Result<()> {
    let sig_url = format!("{asset_url}.sig");
    let fetch = match client.get(&sig_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            match read_body_capped(resp, MAX_SIG_BYTES, None).await {
                Ok(body) => SigFetch::Body(String::from_utf8_lossy(&body).into_owned()),
                Err(e) => SigFetch::Failed(format!("{e:#}")),
            }
        }
        Ok(resp) => SigFetch::Status(resp.status()),
        Err(e) => SigFetch::Failed(e.to_string()),
    };

    match signature_to_verify(version, fetch)? {
        Some(sig_b64) => {
            verify_minisign(pubkey_b64, &sig_b64, bytes)?;
            tracing::info!("update signature verified (minisign 4A0852406D06E275)");
        }
        None => tracing::warn!(
            %sig_url,
            "v{version} predates signed releases (v{FIRST_SIGNED_VERSION}) and has no signature — installing UNVERIFIED"
        ),
    }
    Ok(())
}

/// Read a response body chunk by chunk, refusing anything over `cap` bytes — checked against
/// `Content-Length` up front and against the running total, so a missing or lying header can't
/// grow memory without bound — and anything shorter than its `Content-Length`.
/// `on_progress(downloaded, total)` runs after each chunk.
async fn read_body_capped(
    mut resp: reqwest::Response,
    cap: u64,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> anyhow::Result<Vec<u8>> {
    let content_len = resp.content_length();
    if let Some(len) = content_len {
        if len > cap {
            anyhow::bail!("download is {len} bytes, over the {cap}-byte limit — refusing");
        }
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(content_len.unwrap_or(0) as usize);
    while let Some(chunk) = resp.chunk().await.context("download stream interrupted")? {
        if bytes.len() as u64 + chunk.len() as u64 > cap {
            anyhow::bail!("download exceeded the {cap}-byte limit — refusing");
        }
        bytes.extend_from_slice(&chunk);
        if let Some(cb) = on_progress {
            cb(bytes.len() as u64, content_len);
        }
    }
    if let Some(expected) = content_len {
        if bytes.len() as u64 != expected {
            anyhow::bail!(
                "download truncated: received {} bytes, expected {expected} — aborting to protect \
                 the installed binary",
                bytes.len()
            );
        }
    }
    Ok(bytes)
}

/// Download this platform's CLI asset for release `version` (canonical tag `tag`) from
/// `base_url`, then check it the same way for every caller: size cap, non-empty, a valid
/// signature (fail-closed) and executable magic bytes. `base_url` is the GitHub releases URL in
/// production and a local server in tests.
async fn download_verified_asset(
    client: &Client,
    base_url: &str,
    version: &Version,
    tag: &str,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    pubkey_b64: &str,
) -> anyhow::Result<Vec<u8>> {
    let asset = asset_name()?;
    let url = format!("{base_url}/{tag}/{asset}");
    tracing::info!(%url, "downloading release asset");

    let resp = client
        .get(&url)
        .send()
        .await
        .context("download request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "download failed (HTTP {}): tag={tag} asset={asset}\n\
             Make sure the release exists at https://github.com/{REPO}/releases",
            resp.status()
        );
    }
    let bytes = read_body_capped(resp, MAX_ASSET_BYTES, on_progress).await?;
    if bytes.is_empty() {
        anyhow::bail!("downloaded binary is empty — the release asset may be missing");
    }

    // Cryptographically verify the download before it is installed, and sanity-check the magic
    // bytes (a clearer error for an HTML error page or an LFS pointer).
    verify_asset_signature(client, &url, version, &bytes, pubkey_b64).await?;
    if !looks_like_executable(&bytes) {
        anyhow::bail!(
            "downloaded asset is not a recognized executable (Mach-O/ELF/PE) — refusing to install"
        );
    }
    Ok(bytes)
}

/// Where release assets are downloaded from: `{RELEASES_BASE}/{tag}/{asset}`.
fn releases_base() -> String {
    format!("https://github.com/{REPO}/releases/download")
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
/// `tag` may be `"v0.12.1"` or `"0.12.1"`; it must parse as semver, and the
/// download URL is built from the parsed version (see [`parse_release_tag`]).
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
/// - A `tag` that is not a strict semver version (it is interpolated into the download URL).
/// - A `tag` older than the running binary, unless [`ALLOW_DOWNGRADE_ENV`] is `1`.
/// - Truncated, empty or oversized download.
/// - A missing, unreachable, empty or non-verifying signature for a signed-era release.
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

    let (version, tag_str) = parse_release_tag(tag)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the running binary's version is not semver")?;
    if let Some(reason) = downgrade_refusal(
        &current,
        &version,
        std::env::var(ALLOW_DOWNGRADE_ENV).as_deref() == Ok("1"),
    ) {
        anyhow::bail!(reason);
    }

    let client = build_client()?;
    let bytes = download_verified_asset(
        &client,
        &releases_base(),
        &version,
        &tag_str,
        None,
        MINISIGN_PUBKEY_B64,
    )
    .await?;

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
        let tmp_path = stage_executable(&exe_dir, ".indexa-update-", &bytes)?;

        // Atomically replace the running binary. `self_replace` COPIES `tmp_path` next to the
        // exe and renames that copy into place, so `tmp_path` itself is never consumed; it is a
        // `TempPath`, deleted when it drops here — after success and on every error path alike.
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

    tracing::info!(%version, "update applied — restart to run the new version");
    Ok(version.to_string())
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
    // No downgrade check here: this never replaces the running binary, it installs the CLI that
    // matches the desktop app's own version.
    let (version, tag_str) = parse_release_tag(tag)?;
    let client = build_client()?;
    let bytes = download_verified_asset(
        &client,
        &releases_base(),
        &version,
        &tag_str,
        on_progress,
        MINISIGN_PUBKEY_B64,
    )
    .await?;

    let bin_name = if cfg!(windows) {
        "indexa.exe"
    } else {
        "indexa"
    };
    let dir = dir.to_path_buf();
    let dest = dir.join(bin_name);
    let dest_for_task = dest.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        install_executable(&dir, &dest_for_task, &bytes)?;
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

/// Write `bytes` to a fresh temp file in `dir` (named `{prefix}…`), mark it executable on Unix,
/// and return it as a `TempPath`: the handle is closed, and the file is deleted when the
/// `TempPath` drops unless it is persisted first — so no error path can leave a stray copy of a
/// tens-of-MB binary behind.
fn stage_executable(
    dir: &std::path::Path,
    prefix: &str,
    bytes: &[u8],
) -> anyhow::Result<tempfile::TempPath> {
    let mut tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(dir)
        .map_err(|e| permission_error(e, dir))?;
    tmp.write_all(bytes).context("write to temp file failed")?;
    tmp.flush().context("flush temp file failed")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .context("chmod on temp file failed")?;
    }
    Ok(tmp.into_temp_path())
}

/// Install `bytes` as the executable `dest` inside `dir`: stage a temp file in `dir`, then rename
/// it into place (atomic on the same filesystem). If the rename fails, the temp file is removed.
fn install_executable(
    dir: &std::path::Path,
    dest: &std::path::Path,
    bytes: &[u8],
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| permission_error(e, dir))?;
    let tmp_path = stage_executable(dir, ".indexa-cli-", bytes)?;
    tmp_path
        .persist(dest)
        .map_err(|e| permission_error(e.error, dest))?;
    Ok(())
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
        asset_name, cumulative_changelog, downgrade_refusal, download_verified_asset,
        install_executable, is_inside_app_bundle, looks_like_executable, parse_release_tag,
        read_body_capped, self_replace_refusal, signature_to_verify, stage_executable,
        verify_asset_signature, verify_minisign, SigFetch, ALLOW_DOWNGRADE_ENV,
        FIRST_SIGNED_VERSION, MINISIGN_PUBKEY_B64,
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

    // ── Fail-closed update path ─────────────────────────────────────────────────────────────

    #[test]
    fn release_tags_must_be_strict_semver() {
        assert_eq!(
            parse_release_tag("v0.81.0").unwrap(),
            (Version::new(0, 81, 0), "v0.81.0".to_owned())
        );
        assert_eq!(parse_release_tag("0.81.0").unwrap().1, "v0.81.0");
        assert_eq!(parse_release_tag("v1.2.3-rc.1").unwrap().1, "v1.2.3-rc.1");
        for bad in [
            // Dot-segments would be normalised by the URL parser into another repo's asset.
            "v1/../../../../attacker/repo/releases/download/v9",
            "v0.81.0/../../../evil",
            "v1?x",
            "%2e%2e",
            "v0.81.0%2F..",
            "v0.81",
            "vv0.81.0",
            " v0.81.0",
            "v0.81.0/",
            "",
        ] {
            assert!(parse_release_tag(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn downgrades_are_refused_unless_explicitly_allowed() {
        let current = Version::new(0, 80, 3);
        let older = Version::new(0, 79, 0);
        let reason = downgrade_refusal(&current, &older, false).unwrap();
        assert!(reason.contains(ALLOW_DOWNGRADE_ENV), "{reason}");
        assert!(downgrade_refusal(&current, &older, true).is_none());
        // Same version (reinstall) and upgrades are not downgrades.
        assert!(downgrade_refusal(&current, &current, false).is_none());
        assert!(downgrade_refusal(&current, &Version::new(0, 81, 0), false).is_none());
        // A pre-release of the running version is older than it.
        let rc = Version::parse("0.80.3-rc.1").unwrap();
        assert!(downgrade_refusal(&current, &rc, false).is_some());
    }

    #[test]
    fn signature_policy_fails_closed_for_signed_releases() {
        use reqwest::StatusCode;
        let signed = FIRST_SIGNED_VERSION.clone();
        let legacy = Version::new(0, 76, 0);
        let sig = || SigFetch::Body("c2ln".to_owned());

        assert_eq!(
            signature_to_verify(&signed, sig()).unwrap(),
            Some("c2ln".into())
        );
        for fetch in [
            SigFetch::Status(StatusCode::NOT_FOUND),
            SigFetch::Status(StatusCode::INTERNAL_SERVER_ERROR),
            SigFetch::Status(StatusCode::FORBIDDEN),
            SigFetch::Body(String::new()),
            SigFetch::Body(" \n".to_owned()),
            SigFetch::Failed("connection refused".to_owned()),
        ] {
            let desc = format!("{fetch:?}");
            assert!(
                signature_to_verify(&signed, fetch).is_err(),
                "signed release must not install unverified on {desc}"
            );
        }

        // Only an explicit 404 for a release that predates signing skips verification.
        assert_eq!(
            signature_to_verify(&legacy, SigFetch::Status(StatusCode::NOT_FOUND)).unwrap(),
            None
        );
        assert_eq!(
            signature_to_verify(&legacy, sig()).unwrap(),
            Some("c2ln".into())
        );
        for fetch in [
            SigFetch::Status(StatusCode::BAD_GATEWAY),
            SigFetch::Body(String::new()),
            SigFetch::Failed("timed out".to_owned()),
        ] {
            assert!(signature_to_verify(&legacy, fetch).is_err());
        }
    }

    /// A tiny HTTP/1.1 server on 127.0.0.1: each request gets the canned raw response for its
    /// path (or a 404) and the connection is closed. Returns the base URL.
    async fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = std::sync::Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&req);
                    let path = head.split_whitespace().nth(1).unwrap_or_default();
                    let resp = routes
                        .iter()
                        .find(|(p, _)| p == path)
                        .map(|(_, r)| r.clone())
                        .unwrap_or_else(|| status(404));
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// A client that talks to the local test server directly, whatever `HTTP(S)_PROXY` says.
    fn local_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn status(code: u16) -> Vec<u8> {
        format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
    }

    #[tokio::test]
    async fn missing_errored_or_empty_signature_aborts_a_signed_release() {
        let base = serve(vec![
            ("/a404.sig".into(), status(404)),
            ("/a500.sig".into(), status(500)),
            ("/aempty.sig".into(), ok(b"")),
            ("/alegacy.sig".into(), status(404)),
        ])
        .await;
        let client = local_client();
        let signed = Version::new(0, 80, 3);
        let check = |name: &'static str, v: Version| {
            let client = client.clone();
            let url = format!("{base}/{name}");
            async move {
                verify_asset_signature(&client, &url, &v, TEST_MSG, TEST_PUBKEY)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        };

        let e = check("a404", signed.clone()).await.unwrap_err();
        assert!(e.contains("no update signature is published"), "{e}");
        let e = check("a500", signed.clone()).await.unwrap_err();
        assert!(e.contains("HTTP 500"), "{e}");
        let e = check("aempty", signed.clone()).await.unwrap_err();
        assert!(e.contains("empty"), "{e}");
        // A release predating signing still installs on an explicit 404 (reachable only via an
        // explicitly allowed downgrade).
        check("alegacy", Version::new(0, 76, 0)).await.unwrap();

        // Nothing listening: a network error is not "unsigned".
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/asset", l.local_addr().unwrap())
        };
        let e = verify_asset_signature(&client, &dead, &signed, TEST_MSG, TEST_PUBKEY)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("could not fetch"), "{e:#}");
    }

    #[tokio::test]
    async fn published_signature_is_verified_over_http() {
        let base = serve(vec![("/asset.sig".into(), ok(TEST_SIG_B64.as_bytes()))]).await;
        let client = local_client();
        let url = format!("{base}/asset");
        let v = Version::new(0, 80, 3);
        verify_asset_signature(&client, &url, &v, TEST_MSG, TEST_PUBKEY)
            .await
            .unwrap();
        assert!(
            verify_asset_signature(&client, &url, &v, b"tampered", TEST_PUBKEY)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn download_runs_size_and_signature_checks_end_to_end() {
        let asset = asset_name().unwrap();
        let base = serve(vec![
            // Signed release whose .sig is missing → refused before anything is installed.
            (format!("/v0.80.3/{asset}"), ok(b"\x7fELF-not-really")),
            // Valid signature over a non-executable payload → verified, then rejected by magic.
            (format!("/v0.80.2/{asset}"), ok(TEST_MSG)),
            (format!("/v0.80.2/{asset}.sig"), ok(TEST_SIG_B64.as_bytes())),
        ])
        .await;
        let client = local_client();
        let get = |v: Version| {
            let client = client.clone();
            let base = base.clone();
            async move {
                let tag = format!("v{v}");
                download_verified_asset(&client, &base, &v, &tag, None, TEST_PUBKEY)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        };
        let e = get(Version::new(0, 80, 3)).await.unwrap_err();
        assert!(e.contains("no update signature is published"), "{e}");
        let e = get(Version::new(0, 80, 2)).await.unwrap_err();
        assert!(e.contains("not a recognized executable"), "{e}");
        let e = get(Version::new(0, 80, 1)).await.unwrap_err();
        assert!(e.contains("HTTP 404"), "{e}");
    }

    #[tokio::test]
    async fn body_reads_are_capped_and_truncation_is_caught() {
        let mut no_len = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
        no_len.extend_from_slice(&[b'x'; 64]);
        let mut short =
            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\n".to_vec();
        short.extend_from_slice(&[b'x'; 10]);
        let base = serve(vec![
            ("/declared".into(), ok(&[b'x'; 64])),
            ("/undeclared".into(), no_len),
            ("/short".into(), short),
            ("/fits".into(), ok(&[b'x'; 16])),
        ])
        .await;
        let client = local_client();
        let read = |path: &'static str| {
            let client = client.clone();
            let url = format!("{base}{path}");
            async move {
                let resp = client.get(&url).send().await.unwrap();
                read_body_capped(resp, 16, None).await
            }
        };
        // Declared Content-Length over the cap → refused before reading.
        assert!(read("/declared").await.is_err());
        // No Content-Length, body grows past the cap → refused mid-stream.
        assert!(read("/undeclared").await.is_err());
        // Fewer bytes than declared → truncated.
        assert!(read("/short").await.is_err());
        assert_eq!(read("/fits").await.unwrap().len(), 16);
    }

    fn leftovers(dir: &Path, prefix: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(prefix))
            .collect()
    }

    #[test]
    fn staged_binary_is_deleted_when_dropped() {
        // `apply` hands this to `self_replace`, which copies it rather than moving it — so the
        // staged file must clean itself up (it used to be `keep()`-ed and left behind).
        let dir = tempfile::tempdir().unwrap();
        let staged = stage_executable(dir.path(), ".indexa-update-", b"\x7fELF").unwrap();
        assert_eq!(leftovers(dir.path(), ".indexa-update-").len(), 1);
        drop(staged);
        assert!(leftovers(dir.path(), ".indexa-update-").is_empty());
    }

    #[test]
    fn install_leaves_no_temp_file_on_success_or_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("indexa");
        install_executable(dir.path(), &dest, b"\x7fELF-v1").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"\x7fELF-v1");
        assert!(leftovers(dir.path(), ".indexa-cli-").is_empty());

        // The rename fails when the destination is a non-empty directory.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("keep"), b"x").unwrap();
        assert!(install_executable(dir.path(), &blocked, b"\x7fELF-v2").is_err());
        assert!(leftovers(dir.path(), ".indexa-cli-").is_empty());
    }
}
