# Fingerprints

`indexa fingerprint` detects **software and project types** across your indexed folders by
file-pattern signatures — Rust crates, Node/Next.js apps, Docker Compose stacks, Helm charts, and
more — **without reading file contents**. It runs over the entries a `scan` already recorded, so
it's instant.

```bash
indexa scan ~/code
indexa fingerprint            # summary: type × count
indexa fingerprint --paths    # also list the matching directories
```

## How matching works

A fingerprint matches a directory when **all** of its signature names (`all_of`) are present as
**direct children** of that directory (files or subdirectories). For example, a *Next.js app*
requires both `package.json` and `next.config.js` in the same folder.

## The pattern library

The built-in library covers common ecosystems. You can extend it — no recompile needed — by
creating a `fingerprints.json` next to your config file:

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/dev.indexa.Indexa/fingerprints.json` |
| Linux | `~/.config/indexa/fingerprints.json` |
| Windows | `%APPDATA%\indexa\Indexa\fingerprints.json` |

Entries in that file are **appended** to the built-ins.

### JSON format

```json
[
  {
    "name": "Rails app",
    "category": "code",
    "description": "Ruby on Rails application",
    "all_of": ["Gemfile", "config.ru"]
  },
  {
    "name": "pnpm workspace",
    "category": "code",
    "description": "pnpm monorepo",
    "all_of": ["pnpm-workspace.yaml"]
  }
]
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Display name in the report. |
| `category` | yes | Coarse grouping, e.g. `code` or `infra`. |
| `description` | no | One-line description shown after the name. |
| `all_of` | yes | Direct-child names (files **or** directories) that must **all** be present. |

### Contributing a pattern

To add a pattern to the built-in library, edit `DEFAULT_FINGERPRINTS_JSON` in
[`crates/core/src/fingerprint.rs`](../crates/core/src/fingerprint.rs) and add a test case to the
module's `tests`. Keep signatures to **fixed, unambiguous filenames** (e.g. `Cargo.toml`,
`go.mod`, `Chart.yaml`) so detection stays free of false positives. Prefer an `all_of` combination
when a single common filename (like `package.json`) would over-match.
