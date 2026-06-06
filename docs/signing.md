# macOS code signing & notarization

The Indexa desktop app is built and released by `.github/workflows/release.yml`. When the Apple
secrets below are present, the release is **Developer ID Application signed and notarized** — so it
launches cleanly on macOS (no Gatekeeper warning) and the in-app auto-updater works without any
ad-hoc re-sign workaround. When the secrets are absent, the build falls back to **ad-hoc** signing
(the app still runs locally but isn't notarized).

## Universal binary (Intel + Apple Silicon)

The desktop bundle is built with `--target universal-apple-darwin` — a single
`.dmg`/`.app.tar.gz` that runs natively on both Intel and Apple-Silicon Macs. It
is published under the **`darwin-universal`** key in `latest.json`, and the app
pins its updater to that key (`tauri_plugin_updater::Builder::target("darwin-universal")`
in `apps/indexa-desktop/src/main.rs`). Pre-universal builds (≤ v0.19) queried the
`darwin-aarch64` key, so the first universal release must be **installed manually**
— an old client won't find it via auto-update (this is the same release where the
ad-hoc → Developer-ID signing transition also requires a manual install).

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

## Entitlements

The app runs **without a custom entitlements file**: it is not sandboxed (Developer ID distribution),
so its local HTTP server, subprocess spawning (`ollama`/`ffmpeg`/`whisper-cli`), and file reads need
no entitlements, and WKWebView's JIT runs in a system-provided process. If a notarization log ever
reports a missing entitlement, add an `apps/indexa-desktop/Entitlements.plist` and reference it from
`tauri.conf.json` (`bundle.macOS.entitlements`).
