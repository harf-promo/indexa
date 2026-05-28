# Contributing to Indexa

Thank you for your interest in contributing. This document covers everything you need to get your first patch merged.

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

---

## Code style

- Follow standard Rust idioms and `rustfmt` defaults.
- Prefer existing abstractions (`Embedder`, `Describer` traits) over adding new ones unnecessarily.
- No comments that restate what the code says. Comments should explain *why* something non-obvious is done.
- Error handling: use `anyhow` for application errors, `thiserror` for library crate errors.

---

## Adding a new LLM adapter

1. Implement the `Embedder` and/or `Describer` traits in `crates/embed/src/` or `crates/llm/src/`.
2. Add a `[dependencies]` entry in that crate's `Cargo.toml` and a config struct in `crates/cli/src/config.rs`.
3. Wire the adapter into `apps/indexa/src/main.rs`.
4. Add an integration test that hits a mock HTTP server (see existing adapter tests for the pattern).
5. Document the config options in `docs/config.md`.

---

## Adding a new file parser

1. Implement the `Parser` trait in `crates/parsers/src/`.
2. Register it in `crates/parsers/src/registry.rs`.
3. Add a test with a sample file in `crates/parsers/tests/fixtures/`.

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
