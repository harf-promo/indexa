# indexa-desktop — Tauri desktop app

Native menu-bar wrapper around the Indexa local context engine.
Embeds the Axum web server directly (no subprocess) and opens a
`WebviewWindow` pointing at `http://localhost:7620`.

## Building locally

```bash
# From the repo root:
cargo build --manifest-path apps/indexa-desktop/Cargo.toml
# or release:
cargo build --manifest-path apps/indexa-desktop/Cargo.toml --release
```

### Prerequisites

| Platform | Required |
|---|---|
| macOS | Xcode Command Line Tools (`xcode-select --install`) |
| Linux | `libwebkit2gtk-4.1-dev libgtk-3-dev libappindicator3-dev` |
| Windows | WebView2 Runtime (pre-installed on Windows 10 21H2+) |

> **Why excluded from the workspace?**  
> `cargo build --all` runs on Ubuntu CI runners that lack the webkit2gtk packages.
> The desktop crate is listed in `[workspace] exclude` so the standard workspace
> build commands skip it. Build it explicitly with the `--manifest-path` flag above.

## Workspace exclusion

`apps/indexa-desktop` is excluded from the Cargo workspace (`[workspace] exclude`
in the root `Cargo.toml`). This means:
- `cargo clippy --workspace`, `cargo test --workspace`, and CI all skip it.
- `cargo-deny` doesn't check its transitive deps (Tauri pulls in MPL-2.0 crates).

## Publishing — blockers before a signed release

The local unsigned build (`cargo build`) compiles and launches cleanly.
A properly **published** release requires:

1. **Apple Developer ID** — macOS code-signing + notarization (Xcode + `xcrun notarytool`).
2. **Windows Code Signing Certificate** — Authenticode signing for the `.msi` installer.
3. **Real icons** — the current `icons/` contains placeholder RGBA PNGs.
   Use `tauri icon` (cargo install tauri-cli) to generate all platform formats from
   a 1024×1024 source PNG.
4. **CI / release-matrix additions** — GitHub Actions would need platform-specific
   build steps (apt-install webkit2gtk on Linux, macOS keychain import, etc.) and
   an updated `release.yml` that bundles and uploads the desktop artifacts alongside
   the CLI binaries.

## Architecture

```
main()
 ├─ spawn background thread
 │   └─ Tokio runtime → indexa_web::serve(7620, store, embedder, llm, cfg)
 ├─ wait_for_port(7620, 15s)   — polls TCP before opening the webview
 └─ tauri::Builder
     ├─ setup: tray icon (Show / Quit menu)
     └─ WebviewWindow → http://localhost:7620
```

The embedded server is identical to `indexa serve` — it shares the same
`index.db`, config file, Ollama models, and API keys.
