//! Offline generator for `crates/core/src/fingerprints_seed.json`.
//!
//! Indexa's curated built-in library (`fingerprints_builtin.json`) is hand-authored and covers the
//! famous structures across all four families. This generator adds the **long tail of language /
//! package-manager manifests** by transcribing the project-type catalog of CycloneDX **cdxgen**
//! (Apache-2.0) into Indexa fingerprint defs, each stamped with provenance.
//!
//! It is deliberately a maintainer-run, OFFLINE step (not in `cargo build`, not in CI): run it by
//! hand when the catalog changes and commit the resulting snapshot. The Indexa runtime only ever
//! `include_str!`s the committed JSON — it never fetches anything.
//!
//!   cargo run --manifest-path tools/gen-fingerprints/Cargo.toml
//!
//! To refresh from a newer cdxgen, update `CDXGEN_VERSION` and the `MANIFESTS` table below from
//! cdxgen's documented project types (https://github.com/CycloneDX/cdxgen, Apache-2.0).

use serde::Serialize;
use std::path::Path;

/// The cdxgen release this transcription reflects. Bump when you refresh `MANIFESTS`.
const CDXGEN_VERSION: &str = "cdxgen-project-types@2025.x (manual transcription)";

/// (display name, stable kind, manifest marker) — the long-tail manifests not already covered by
/// the curated built-in library. Each becomes an `all_of: [marker]`, family "code".
const MANIFESTS: &[(&str, &str, &str)] = &[
    ("Swift package", "swift_package", "Package.swift"),
    ("CocoaPods project", "cocoapods_project", "Podfile"),
    ("Elixir / Mix project", "elixir_mix", "mix.exs"),
    ("Elm project", "elm_project", "elm.json"),
    ("Haskell (Stack) project", "haskell_stack", "stack.yaml"),
    ("Scala (sbt) project", "sbt_project", "build.sbt"),
    ("Clojure (deps) project", "clojure_deps", "deps.edn"),
    ("Clojure (Leiningen) project", "leiningen_project", "project.clj"),
    ("Perl distribution", "perl_dist", "cpanfile"),
    ("Conda environment", "conda_env", "environment.yml"),
    ("Pipenv project", "pipenv_project", "Pipfile"),
    ("Conan (C/C++) project", "conan_project", "conanfile.txt"),
    ("vcpkg (C/C++) project", "vcpkg_project", "vcpkg.json"),
    ("CMake project", "cmake_project", "CMakeLists.txt"),
    ("Meson project", "meson_project", "meson.build"),
    ("Bazel workspace", "bazel_workspace", "MODULE.bazel"),
    ("Nim package", "nim_package", "config.nims"),
    ("Crystal shard", "crystal_shard", "shard.yml"),
    ("Zig project", "zig_project", "build.zig"),
    ("PHP / Composer project", "php_composer", "composer.json"),
    ("Julia project", "julia_project", "Project.toml"),
    ("OCaml (Dune) project", "ocaml_dune", "dune-project"),
    ("Erlang (rebar3) project", "erlang_rebar", "rebar.config"),
];

#[derive(Serialize)]
struct Provenance {
    source: String,
    license: String,
    version: String,
}

#[derive(Serialize)]
struct SeedDef {
    name: String,
    category: String,
    family: String,
    kind: String,
    specificity: u32,
    description: String,
    all_of: Vec<String>,
    provenance: Provenance,
}

fn main() -> std::io::Result<()> {
    let defs: Vec<SeedDef> = MANIFESTS
        .iter()
        .map(|(name, kind, marker)| SeedDef {
            name: (*name).to_owned(),
            category: "code".to_owned(),
            family: "code".to_owned(),
            kind: (*kind).to_owned(),
            specificity: 10,
            description: format!("{marker} manifest"),
            all_of: vec![(*marker).to_owned()],
            provenance: Provenance {
                source: "CycloneDX cdxgen project-types".to_owned(),
                license: "Apache-2.0".to_owned(),
                version: CDXGEN_VERSION.to_owned(),
            },
        })
        .collect();

    // Resolve the output relative to this crate, so it works regardless of CWD.
    let out = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/core/src/fingerprints_seed.json");
    let json = serde_json::to_string_pretty(&defs).expect("serialize seed defs");
    std::fs::write(&out, format!("{json}\n"))?;
    eprintln!(
        "wrote {} seeded fingerprint defs to {}",
        defs.len(),
        out.display()
    );
    Ok(())
}
