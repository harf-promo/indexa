# Contributing to Indexa

Indexa is building the local context engine for AI — private, fast, and yours. If you care about
AI that runs on your own hardware and respects your data, we'd love your help. This document covers
everything you need to get your first patch merged.

---

## Before you start

- Check [Issues](../../issues) for existing reports before opening a new one.
- For significant new features or architectural changes, open a Discussion first to align before writing code.
- Items labeled [`good first issue`](../../issues?q=label%3A%22good+first+issue%22) are intentionally scoped for new contributors.

---

## Developer setup

### Requirements

- **Rust** ≥ 1.82 — install via [rustup](https://rustup.rs/)
- **Git** ≥ 2.34
- For the `parsers` crate on macOS/Linux: `ffprobe` (part of ffmpeg) for audio/video metadata

> **PATH note:** `rustup` installs Cargo to `~/.cargo/bin`. If `cargo` is not found after installation, add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell profile (`~/.zshrc`, `~/.bashrc`, etc.) and restart your terminal. On macOS with a default shell, `~/.cargo/bin` is often missing from PATH in non-login shells.

```bash
# Clone
git clone https://github.com/harf-promo/indexa
cd indexa

# Build all crates
cargo build

# Run all tests
cargo test

# Check formatting and lints
cargo fmt --check
cargo clippy -- -D warnings
```

### Running locally

```bash
# Run the CLI directly
cargo run -p indexa -- scan ~/Documents
cargo run -p indexa -- ask "where are my tax documents?"
cargo run -p indexa -- serve
```

---

## Making changes

1. **Fork** the repo and create a branch: `git checkout -b my-feature`
2. Make your changes.
3. Add or update tests where applicable.
4. Run `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` — all must pass.
5. **Sign off your commit** (see below).
6. Open a pull request against `main`.

---

## Developer Certificate of Origin (DCO)

All commits must be signed off with the Developer Certificate of Origin. This certifies that you wrote the code or have the right to submit it under the Apache-2.0 license.

Add a sign-off to every commit:

```bash
git commit -s -m "your commit message"
```

This appends `Signed-off-by: Your Name <your@email.com>` to the commit message. The DCO bot will check this on every PR. Without a sign-off, the PR cannot be merged.

If you forgot to sign off on past commits in your branch:

```bash
git rebase HEAD~<number-of-commits> --signoff
git push --force-with-lease
```

Full DCO text: https://developercertificate.org/

---

## Pull request checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test` passes
- [ ] All commits have `Signed-off-by:`
- [ ] PR description explains *what* and *why* (not just what the diff shows)
- [ ] New public API has doc comments; new behaviour has at least one test
- [ ] If you touched `apps/indexa-desktop/`: `cargo build --manifest-path apps/indexa-desktop/Cargo.toml` passes and `Cargo.lock` is committed

---

## Code style

- Follow standard Rust idioms and `rustfmt` defaults.
- Prefer existing abstractions (`Embedder`, `Describer` traits) over adding new ones unnecessarily.
- No comments that restate what the code says. Comments should explain *why* something non-obvious is done.
- Error handling: use `anyhow` for application errors, `thiserror` for library crate errors.

---

## Adding a new LLM adapter

1. Implement the `Embedder` and/or `Describer` traits in `crates/embed/src/` or `crates/llm/src/`.
2. Add a `[dependencies]` entry in that crate's `Cargo.toml` and a config struct in `crates/core/src/config.rs`.
3. Wire the adapter into `apps/indexa/src/main.rs`.
4. Add an integration test that hits a mock HTTP server (see existing adapter tests for the pattern).
5. Document the config options in `docs/config.md`.

---

## Adding a new file parser

1. Implement the `Parser` trait in `crates/parsers/src/`.
2. Register it in `crates/parsers/src/registry.rs`.
3. Add a test with a sample file in `crates/parsers/tests/fixtures/`.

This section is for parsers that ship **in** the `indexa` binary itself. If you're
publishing your own parser as a separate crate for other people to use, see the next
section instead.

---

## Authoring a third-party parser plugin

Indexa's parser "Plugin SDK" is a **compile-time** Rust extension point — there is no
dynamic/runtime plugin loading (no `dlopen`, no WASM). A third-party parser is a crate
that implements `indexa_parsers::types::Parser` and is registered into
`indexa_parsers::registry::Registry` from **your own custom binary** that depends on
`indexa_parsers` as a library. It never modifies the stock `indexa` binary.

1. **Implement `Parser`.** In your own crate:

   ```rust
   use indexa_parsers::registry::Registry;
   use indexa_parsers::types::{Extracted, Parser};

   struct MyParser;
   impl Parser for MyParser {
       fn accepts_mime(&self, mime: &str) -> bool { mime == "application/x-mything" }
       fn parse(&self, path: &std::path::Path) -> anyhow::Result<Extracted> {
           // ... read the file and return chunks ...
           Ok(Extracted { source: path.to_path_buf(), mime: "application/x-mything".into(),
               chunks: vec![], edges: vec![] })
       }
   }
   ```

   The full contract (including `accepts_path`, chunking, and precedence rules) is in
   `crates/parsers/src/registry.rs`'s "Plugin SDK" doc comment — read that first.

2. **Register it in your own binary**, before parsing:

   ```rust
   let mut reg = Registry::new();
   reg.register(Box::new(MyParser));
   let extracted = reg.parse(path)?;
   ```

3. **Publish the crate** to crates.io (or a git repo) under your own name/license.

4. **List it in the plugin directory** so others can find it: open a PR appending a
   `[[plugin]]` table to `crates/parsers/plugins.toml` with `name`, `crate_name`,
   `parser_type` (the type callers should `Box::new(...)`), `description`,
   `extensions`/`mime_types`, and `repo`. See the comment header in that file for the
   exact shape. Once merged, `indexa plugin list` and `indexa plugin info <name>` (see
   `apps/indexa/src/commands/plugin.rs`) surface it, including the ready-to-paste
   dependency line and registration snippet for your specific crate. Entries are
   curated by maintainers (accuracy, license sanity, upkeep) rather than an open,
   unmoderated registry.

   The directory ships embedded in the `indexa` binary and is read with no network
   call — a future version may additionally fetch a remote curated list, but that's
   not built yet.

---

## Adding an MCP tool

Tools are methods on `IndexaMcp`, grouped into router modules under `crates/mcp/src/` — **not**
defined in `lib.rs`. See the step-by-step guide:
[docs/how-to/add-an-mcp-tool.md](docs/how-to/add-an-mcp-tool.md). In short: add a
`#[tool(description = …)]` method to the right router module, then
`INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp`, commit `golden_tools.txt`, and bump the tool
count in `README.md` / `CLAUDE.md` (a contract test enforces both).

---

## Continuous integration

Every pull request to `main` must pass these checks before it can merge:

- **`fmt + clippy + test`** on Ubuntu, macOS, and Windows.
- **`License and advisory check`** (`cargo-deny`).
- **`DCO sign-off check`**.
- **`web smoke (headless Chrome)`** — boots `indexa serve` and exercises the UI.
- **`desktop build (macOS)`** — the Tauri app is **excluded from `cargo --workspace`** (its
  webkit deps aren't on the Linux CI runners), so it has its own job. If you touch
  `apps/indexa-desktop/`, build it locally first:

  ```bash
  cargo build --manifest-path apps/indexa-desktop/Cargo.toml
  ```

  Keep `apps/indexa-desktop/Cargo.lock` committed and current (the CI builds `--locked`).

---

## Reporting bugs

Use the **Bug report** issue template. Include:
- Indexa version (`indexa --version`)
- OS and architecture
- Steps to reproduce
- What you expected vs what happened

---

## Reporting security vulnerabilities

Do **not** open a public issue. See [SECURITY.md](SECURITY.md).

---

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.
