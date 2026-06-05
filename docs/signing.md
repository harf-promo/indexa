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
| `APPLE_ID` | your Apple Developer account email (notarization) |
| `APPLE_PASSWORD` | an **app-specific password** (notarization) — *not* your Apple ID password |
| `APPLE_TEAM_ID` | your 10-character Team ID |

Already configured (keep — these sign the *updater* artifact, separate from Apple signing):
`TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

> Alternative notarization via App Store Connect API key: instead of `APPLE_ID`/`APPLE_PASSWORD`/
> `APPLE_TEAM_ID`, set `APPLE_API_ISSUER`, `APPLE_API_KEY` (key ID), and `APPLE_API_KEY_PATH`
> (path to the `.p8`). The workflow wires both; populate only one set.

## How to obtain each value

**The `.p12` certificate (`APPLE_CERTIFICATE` + password):**
1. In **Xcode → Settings → Accounts** (or the Apple Developer portal), create/download a
   **Developer ID Application** certificate so it lands in your login Keychain.
2. Open **Keychain Access**, find *Developer ID Application: …*, right-click → **Export**, save as
   `cert.p12`, set an export password (→ `APPLE_CERTIFICATE_PASSWORD`).
3. base64-encode it: `base64 -i cert.p12 | pbcopy` → paste as `APPLE_CERTIFICATE`.

**`APPLE_SIGNING_IDENTITY`:** `security find-identity -v -p codesigning` → copy the quoted name,
e.g. `Developer ID Application: Your Name (ABCDE12345)`.

**`APPLE_TEAM_ID`:** Apple Developer → **Membership** (the 10-char Team ID), or the parenthesized
suffix of the identity string above.

**`APPLE_PASSWORD` (app-specific password):** [appleid.apple.com](https://appleid.apple.com) →
Sign-In and Security → **App-Specific Passwords** → generate one for "Indexa notarization".

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
