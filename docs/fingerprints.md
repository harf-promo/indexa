# Fingerprints — application & structure recognition

Indexa recognizes when a **directory** is a known application or structure — a Rust crate, a
Next.js app, a Django project, a macOS `.app` bundle, a Terraform module, a Jupyter project, and many
more — by matching file-pattern signatures, **without reading file contents**. Detection runs
automatically during `indexa index` and is **persisted as part of the index**, so it shows up
wherever you consume the index:

- `indexa inspect <dir>` — an **App** line ("what kind of thing is this folder?")
- `indexa describe` / `ask` — the project overview tells a broad answer "this folder is a Django app"
- the web **Indexed facts** panel and MCP `inspect` / `project_overview`
- `indexa fingerprint` — the explicit catalogue view (type × count, `--paths` to list directories)

```bash
indexa index ~/code            # detection runs as part of indexing, and is stored
indexa fingerprint             # summary: type × count
indexa fingerprint --paths     # also list the matching directories
```

`indexa fingerprint` reads the persisted detections; if none are stored yet (e.g. you only ran
`scan`), it computes them live over the indexed entries so the command still works.

## How matching works

A fingerprint def matches a directory when its **marker expressions** are satisfied. A marker's
*shape* selects how it's tested:

| Marker form | Example | Matches |
|---|---|---|
| direct child | `Cargo.toml` | a file/dir named exactly that, directly in the dir |
| child glob | `*.xcodeproj` | a direct child whose **name** matches the `*`/`?` glob |
| relative path | `Contents/Info.plist` | a nested entry at that path under the dir |

A def can combine:

- **`all_of`** — every marker must match (the common case).
- **`any_of`** — at least one must match.
- **`none_of`** — anti-markers: if any matches, the def does **not** fire (e.g. a Terraform module
  but not a `.terraform/` cache directory).

When several defs match the same directory (e.g. a Next.js app is also a Node package), all are
recorded, and the one with the highest **`specificity`** becomes the *primary* shown in summaries.

> `**` recursive globs are unsupported (kept out so matching stays linear); a glob inside a relative
> path is treated literally — use a standalone child glob like `*.xcodeproj` instead.

## The pattern library

Definitions come from three places, concatenated in order:

1. **Curated built-ins** — `crates/core/src/fingerprints_builtin.json`, hand-authored across four
   families: `code` (languages, frameworks, CMS), `os` (app bundles, packages), `infra` (containers,
   IaC, CI), `data` (documents, datasets).
2. **Seeded snapshot** — `crates/core/src/fingerprints_seed.json`, generated **offline** from
   CycloneDX [cdxgen](https://github.com/CycloneDX/cdxgen)'s project-type catalogue (Apache-2.0) for
   the long tail of language manifests. Each seeded rule carries `provenance` (source, license,
   version). The runtime **never fetches anything** — the snapshot is committed; a maintainer
   regenerates it with `cargo run --manifest-path tools/gen-fingerprints/Cargo.toml`.
3. **Your catalogue** — an optional `fingerprints.json` next to your config file, **appended** to the
   above (no recompile):

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/dev.indexa.Indexa/fingerprints.json` |
| Linux | `~/.config/indexa/fingerprints.json` |
| Windows | `%APPDATA%\indexa\Indexa\fingerprints.json` |

### JSON format

```json
[
  {
    "name": "Rails app",
    "category": "code",
    "family": "code",
    "kind": "rails_app",
    "specificity": 30,
    "description": "Ruby on Rails application",
    "all_of": ["Gemfile", "config/application.rb"]
  },
  {
    "name": "Terraform module",
    "category": "infra",
    "family": "infra",
    "specificity": 12,
    "any_of": ["*.tf"],
    "none_of": [".terraform"]
  }
]
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Display name. |
| `category` | yes | Legacy coarse grouping; `family` supersedes it for the taxonomy. |
| `family` | no | `code` \| `os` \| `infra` \| `data` (defaults to `category`). |
| `kind` | no | Stable machine id (defaults to a slug of `name`); must be unique across the library. |
| `specificity` | no | Ranking for most-specific-wins (default 10). |
| `description` | no | One-line description shown after the name. |
| `all_of` | no | Markers that must **all** match. |
| `any_of` | no | Markers of which **≥1** must match. |
| `none_of` | no | Anti-markers: if any matches, the def does not fire. |

A def needs at least one positive marker (`all_of` or `any_of`); the pre-v0.66 shape (just `name`,
`category`, `all_of`) still parses unchanged.

### Contributing a pattern

To add a built-in, edit `crates/core/src/fingerprints_builtin.json` (and, for grammar changes, add a
test to `crates/core/src/fingerprint.rs`'s `tests`). Keep signatures to **fixed, unambiguous
markers** so detection stays free of false positives; use `all_of`/`specificity` rather than a single
over-matching filename, and `none_of` to suppress known false positives.
