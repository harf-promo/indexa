# macOS code signing & notarization

The Indexa desktop app is built and released by `.github/workflows/release.yml`. When the Apple
secrets below are present, the release is **Developer ID Application signed and notarized** — so it
launches cleanly on macOS (no Gatekeeper warning) and the in-app auto-updater works without any
ad-hoc re-sign workaround. When the secrets are absent, the build falls back to **ad-hoc** signing
(the app still runs locally but isn't notarized).

## Universal binary (Intel + Apple Silicon)

The desktop bundle is built with `--target universal-apple-darwin` — a single
`.dmg`/`.app.tar.gz` that runs natively on both Intel and Apple-Silicon Macs.
`tauri-action` publishes that one universal artifact under **both** per-arch
updater keys in `latest.json` (`darwin-aarch64` **and** `darwin-x86_64`), so the
app's **default** per-arch updater target resolves correctly on either
architecture — each arch queries its own key and gets the same universal bundle.
(Do **not** pin `.target("darwin-universal")`: tauri never emits that key, so
pinning it makes the updater find no update.) **Install v0.20.1 manually** — it
carries the ad-hoc → Developer-ID signing transition (an older client won't
auto-update across it); clean auto-update works from v0.20.1 onward.

> **v0.20.0 was withdrawn** (release + tag deleted). Its arm64 binary crashed at
> launch — it dynamically linked Homebrew's `libpcre2`, which the hardened runtime
> rejects (see *Self-contained binary* below). **v0.20.1** is the first release that
> signs, notarizes, **and** launches.

## Self-contained binary (static libpcre2)

macOS release binaries must not depend on Homebrew dylibs: under the hardened
runtime, **library validation refuses to load a dylib signed by a different Team
ID**, so a Homebrew dependency aborts the notarized app at launch (DYLD "Library
missing"). The crate at risk is `pcre2` (pulled in by `hyperpolyglot` for
file-type classification): `pcre2-sys` links a pkg-config **system** libpcre2
(Homebrew's, on the arm64 CI runner) unless forced static. The repo-root
[`.cargo/config.toml`](../.cargo/config.toml) sets `PCRE2_SYS_STATIC = 1`, which
compiles the **vendored** pcre2 into the binary on every target.

**Verify any macOS release binary is self-contained:**

```bash
otool -L <binary>    # must list ONLY /usr/lib/* and /System/* — no /opt/homebrew/*
```

(If a future dependency adds another C-FFI `-sys` crate, re-check with `otool -L`:
the same Homebrew-dylib trap applies to any of them.)

## Required GitHub repository secrets

Add these under **Settings → Secrets and variables → Actions** (or `gh secret set NAME --repo
harf-promo/indexa`):

| Secret | What it is |
|---|---|
| `APPLE_CERTIFICATE` | base64 of your **Developer ID Application** certificate exported as a `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the password you set when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | the identity string, e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_API_ISSUER` | App Store Connect API **Issuer ID** (UUID, shown above the keys table) |
| `APPLE_API_KEY` | App Store Connect API **Key ID** (10-char, shown in the keys table) |
| `APPLE_API_KEY_FILE` | base64 of the downloaded **`AuthKey_<KEYID>.p8`** private key file |

Already configured (keep — these sign the *updater* artifact, separate from Apple signing):
`TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

> **Why `APPLE_API_KEY_FILE` instead of `APPLE_API_KEY_PATH`?** `notarytool` requires a real file on
> disk; a secret can only carry a string, not a path that exists. The workflow decodes the base64 to
> `~/private_keys/AuthKey_<KEYID>.p8` at build time and exports the path via `$GITHUB_ENV`.
> For local builds you can still set `APPLE_API_KEY_PATH` directly in your shell.

> **Apple ID alternative:** set `APPLE_ID` (account email), `APPLE_PASSWORD` (an app-specific
> password from appleid.apple.com), and `APPLE_TEAM_ID` instead of the three API-key secrets above.
> The workflow wires both methods; populate only one set.

## How to obtain each value

**The `.p12` certificate (`APPLE_CERTIFICATE` + password):**
1. In **Xcode → Settings → Accounts** (or the Apple Developer portal), create/download a
   **Developer ID Application** certificate so it lands in your login Keychain.
2. Open **Keychain Access**, find *Developer ID Application: …*, right-click → **Export**, save as
   `cert.p12`, set an export password (→ `APPLE_CERTIFICATE_PASSWORD`).
3. base64-encode it: `base64 -i cert.p12 | pbcopy` → paste as `APPLE_CERTIFICATE`.

**`APPLE_SIGNING_IDENTITY`:** `security find-identity -v -p codesigning` → copy the quoted name,
e.g. `Developer ID Application: Your Name (ABCDE12345)`.

**App Store Connect API key (`APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_FILE`):**
1. [appstoreconnect.apple.com](https://appstoreconnect.apple.com) → Users and Access →
   **Integrations → App Store Connect API** → generate a **Team** key (role: *Developer* or higher).
2. Copy the **Issuer ID** (UUID above the keys table) → `APPLE_API_ISSUER`.
3. Copy the **Key ID** (10 chars in the table) → `APPLE_API_KEY`.
4. **Download** `AuthKey_<KEYID>.p8` (one-time only — save it somewhere safe).
5. base64-encode it: `base64 -i AuthKey_<KEYID>.p8 | pbcopy` → paste as `APPLE_API_KEY_FILE`.

## Verifying a build (on macOS, after a signed release)

```bash
codesign -dv --verbose=4 /Applications/Indexa.app    # → Authority=Developer ID Application: …
spctl -a -vvv -t exec /Applications/Indexa.app       # → accepted, source=Notarized Developer ID
stapler validate /Applications/Indexa.app            # → The validate action worked!
```

The release workflow runs exactly these three assertions on every signed build
(the "Assert the bundle is Developer-ID signed, notarized + stapled" step in
`.github/workflows/release.yml`), so a regression to an un-stapled or ad-hoc
bundle fails the desktop job rather than shipping. **Stapling matters:** an
un-stapled notarized app needs the network to validate on first launch, and the
in-app updater extracts the bundle offline — a missing staple would brick it.

### Never re-sign a notarized bundle after the fact

`codesign --force --sign -` (ad-hoc) on an installed app **strips** its Developer-ID
signature and notarization, after which Gatekeeper rejects the (still-quarantined)
bundle and it won't launch. The desktop's post-update re-sign is therefore *fail-closed*
— it only ad-hoc-signs a bundle it can **positively confirm** is already ad-hoc/unsigned
(see `resign_app_bundle` in `apps/indexa-desktop/src/main.rs`). Relatedly, the binary
self-replace updater (`indexa update` / `crates/update`) **refuses to run inside a `.app`
bundle**: it would download the headless CLI binary over the GUI Mach-O and re-sign it
ad-hoc. The desktop updates only through its built-in Tauri updater (menu-bar "Check for
Updates…"). This is the v0.25.0 → v0.25.1 fix.

## Troubleshooting: the **first** notarization can stall for a day or two

The very first time a team notarizes, Apple has to **provision the team for the Notary service** on
their back end. Until they do, `notarytool` submissions sit at **"In Progress" indefinitely** (often
24–48 h, sometimes longer) and never reach *Accepted* or *Invalid*. The tell-tale signs:

- `xcrun notarytool log <submission-id> …` returns **"Submission log is not yet available"** — i.e.
  processing never even started. (A genuine *rejection* would instead return a log with reasons.)
- `xcrun notarytool history …` shows every recent submission still **In Progress**.
- Some toolchains surface it as error **7000 "Team is not yet configured for notarization."**

This is **not** a build, signing, certificate, or entitlements problem — the bundle is fine. It is an
Apple-account state, so:

1. **Do not keep resubmitting.** The block is account-level; new submissions queue behind the same
   wall and stay stuck (resubmitting only helps *after* the team is provisioned).
2. **Confirm it's the provisioning stall**, not a real rejection:
   ```bash
   xcrun notarytool log <submission-id> --key /path/to/AuthKey_<KEYID>.p8 \
     --key-id <KEYID> --issuer <ISSUER-UUID>      # → "Submission log is not yet available"
   xcrun notarytool history --key /path/to/AuthKey_<KEYID>.p8 \
     --key-id <KEYID> --issuer <ISSUER-UUID>      # → all "In Progress"
   ```
3. **Contact Apple Developer *Programs* Support** (the Account Holder) — *not* DTS or Feedback
   Assistant, which can't change account provisioning. **developer.apple.com/contact →
   Development and Technical → Other Development or Technical Questions.** Tell them you're notarizing
   for the first time, give the **Team ID**, the App Store Connect **Issuer ID** + **Key ID**, and a
   stuck submission UUID, and ask them to **enable/configure the team for the Notary service**.
4. Once they confirm, **re-trigger one notarization** (re-run the release workflow, or
   `notarytool submit … --wait` locally). It should reach **Accepted in under ~4 minutes**, and stays
   fast for every release after.

The release workflow prints `notarytool history` automatically on failure (an `if: failure()` step)
so a future stall is self-diagnosing in the job log.

References: Apple Developer Forums threads
[739751](https://developer.apple.com/forums/thread/739751),
[809228](https://developer.apple.com/forums/thread/809228),
[770236](https://developer.apple.com/forums/thread/770236).

## Entitlements

The app runs **without a custom entitlements file**: it is not sandboxed (Developer ID distribution),
so its local HTTP server, subprocess spawning (`ollama`/`ffmpeg`/`whisper-cli`), and file reads need
no entitlements, and WKWebView's JIT runs in a system-provided process. If a notarization log ever
reports a missing entitlement, add an `apps/indexa-desktop/Entitlements.plist` and reference it from
`tauri.conf.json` (`bundle.macOS.entitlements`).
