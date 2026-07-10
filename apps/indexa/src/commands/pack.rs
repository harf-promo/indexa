use anyhow::{bail, Context, Result};
use indexa_core::{
    config::{Config, HybridMode},
    store::Store,
};
use indexa_query::{
    build_export_filter, build_tree, prune_tree, render_json, render_markdown, render_xml,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::helpers::{
    build_embedder, expand, finalize_export, index_db_path, now_unix, require_index_db, ExportSink,
};

pub(crate) async fn cmd_pack_create(
    name: String,
    description: Option<String>,
    auto: bool,
    yes: bool,
    limit: usize,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let id = store.create_pack(&name, description.as_deref())?;
    println!("Created pack \"{name}\" (id: {id})");

    if !auto {
        println!("Add paths with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }

    // ── Auto-suggest paths ────────────────────────────────────────────────────
    println!("Searching for paths related to \"{name}\"…");

    // Try semantic search (requires embedder + summarised tree with embeddings).
    let candidates: Vec<String> = match build_embedder(cfg, None) {
        Ok(embedder) => match embedder.embed(&name).await {
            Ok(embedding) => {
                let hits = store.summary_cosine_search(&embedding, limit, 0.15)?;
                if hits.is_empty() {
                    eprintln!("  (no summary embeddings found — falling back to keyword search)");
                    keyword_suggest(&store, &name, limit)?
                } else {
                    println!("  [semantic match — {} candidates]", hits.len());
                    hits.into_iter().map(|(path, _score)| path).collect()
                }
            }
            Err(e) => {
                eprintln!("  (embedding failed: {e:#} — falling back to keyword search)");
                keyword_suggest(&store, &name, limit)?
            }
        },
        Err(e) => {
            eprintln!("  (embedder unavailable: {e:#} — falling back to keyword search)");
            keyword_suggest(&store, &name, limit)?
        }
    };

    if candidates.is_empty() {
        println!("No related paths found. Add manually with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }

    println!("\nSuggested paths ({}):", candidates.len());
    for p in &candidates {
        println!("  {p}");
    }

    // ── Confirm ───────────────────────────────────────────────────────────────
    let confirmed = if yes {
        true
    } else {
        use std::io::IsTerminal as _;
        if std::io::stdin().is_terminal() {
            print!(
                "\nAdd all {} paths to pack \"{name}\"? [Y/n] ",
                candidates.len()
            );
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().is_empty() || input.trim().to_lowercase() == "y"
        } else {
            true // non-interactive: accept
        }
    };

    if !confirmed {
        println!("Skipped. Add manually with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }

    store.add_pack_paths(&id, &candidates)?;
    println!(
        "Added {} path{} to \"{name}\".",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Keyword fallback for `--auto` when embeddings are unavailable.
fn keyword_suggest(store: &Store, query: &str, limit: usize) -> Result<Vec<String>> {
    let hits = store.hybrid_search(query, None, &HybridMode::Sparse, None, limit * 3, 0.0)?;
    println!("  [keyword match — {} chunk hits]", hits.len());
    let mut seen = std::collections::HashSet::new();
    let paths: Vec<String> = hits
        .into_iter()
        .filter_map(|h| {
            if seen.insert(h.entry_path.clone()) {
                Some(h.entry_path)
            } else {
                None
            }
        })
        .take(limit)
        .collect();
    Ok(paths)
}

pub(crate) async fn cmd_pack_add(name: String, paths: Vec<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store.pack_by_name(&name)?.ok_or_else(|| {
        anyhow::anyhow!("no pack named \"{name}\" — create it first with `indexa pack create`")
    })?;
    let expanded: Vec<String> = paths.iter().map(|p| expand(p)).collect();
    store.add_pack_paths(&pack.id, &expanded)?;
    println!(
        "Added {} path{} to \"{}\".",
        expanded.len(),
        if expanded.len() == 1 { "" } else { "s" },
        name
    );
    Ok(())
}

/// Fetch a remote source (GitHub issue/PR or web page), cache it as a local Markdown file, and add
/// that file to the pack. Network access is gated by `[sources] enabled` / `INDEXA_REMOTE_FETCH_ALLOW`.
pub(crate) async fn cmd_pack_add_url(
    name: String,
    url: String,
    label: Option<String>,
    cfg: &Config,
) -> Result<()> {
    use super::sources;

    if !sources::remote_fetch_allowed(&cfg.sources) {
        bail!(
            "Remote fetching is off. Enable it with `[sources]\\nenabled = true` in config.toml, \
             or set INDEXA_REMOTE_FETCH_ALLOW=1 for this run. (Fetching reaches the network, so \
             it's opt-in.)"
        );
    }
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store.pack_by_name(&name)?.ok_or_else(|| {
        anyhow::anyhow!("no pack named \"{name}\" — create it first with `indexa pack create`")
    })?;

    println!("Fetching {url} …");
    let md = sources::fetch_source_markdown(&url, &cfg.sources).await?;
    let data_dir = indexa_core::config::default_data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    let path = sources::cache_source(&data_dir, &url, label.as_deref(), &md)?;
    let path_str = path.to_string_lossy().into_owned();
    store.add_pack_paths(&pack.id, std::slice::from_ref(&path_str))?;

    println!("Cached {} bytes → {path_str}", md.len());
    println!(
        "Added to pack \"{name}\". Run `indexa index \"{path_str}\"` to make it searchable, \
         then `indexa pack export \"{name}\"`."
    );
    Ok(())
}

pub(crate) async fn cmd_pack_remove(name: String, paths: Vec<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let expanded: Vec<String> = paths.iter().map(|p| expand(p)).collect();
    store.remove_pack_paths(&pack.id, &expanded)?;
    println!("Removed {} path(s) from \"{}\".", expanded.len(), name);
    Ok(())
}

pub(crate) async fn cmd_pack_list() -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let packs = store.list_packs()?;
    if packs.is_empty() {
        println!("No Context Packs yet.");
        println!("Create one with: indexa pack create \"<name>\"");
        return Ok(());
    }
    println!("{:<20} {:>6}  Description", "Name", "Paths");
    println!("{}", "─".repeat(60));
    for p in &packs {
        let desc = p.description.as_deref().unwrap_or("—");
        println!("{:<20} {:>6}  {}", p.name, p.path_count, desc);
    }
    Ok(())
}

pub(crate) async fn cmd_pack_show(name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let paths = store.pack_paths(&pack.id)?;
    if paths.is_empty() {
        println!("Pack \"{name}\" is empty.");
        println!("Add paths with: indexa pack add \"{name}\" <paths…>");
        return Ok(());
    }
    let desc = pack
        .description
        .as_deref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    println!("Pack \"{name}\"{desc} ({} paths):", paths.len());
    for p in &paths {
        println!("  {p}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_pack_export(
    name: String,
    format: String,
    output: Option<String>,
    depth: Option<usize>,
    include_weights: bool,
    signatures: bool,
    token_budget: Option<usize>,
    strict_budget: bool,
    clipboard: bool,
    strip_comments: bool,
    no_redact: bool,
    changed_since: Option<String>,
    category: Option<String>,
) -> Result<()> {
    // Like `indexa export`, a pack export must produce a valid artifact or fail loudly — never
    // a silent stdout notice that gets written into a piped file with a zero exit.
    let db_path = index_db_path()?;
    if !db_path.exists() {
        bail!("No index found. Run `indexa index <path>` first.");
    }
    let store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let paths = store.pack_paths(&pack.id)?;
    if paths.is_empty() {
        bail!("Pack \"{name}\" has no paths. Add paths first with `indexa pack add`.");
    }

    // Relational slice (v0.60): same `--changed-since` / `--category` filters as `indexa export`,
    // shared via `build_export_filter`. `None` ⇒ export the whole pack.
    let now_secs = now_unix();
    let now = now_secs.to_string(); // string form for the <context generated="…"> attribute
    let allow = build_export_filter(
        &store,
        changed_since.as_deref(),
        category.as_deref(),
        now_secs,
    )?;
    let mut out_buf = String::new();
    let is_xml = format != "md" && format != "markdown" && format != "json";

    // XML: wrap all roots in a single <context> element for a self-contained file
    if is_xml {
        out_buf.push_str("<context pack=\"");
        out_buf.push_str(&indexa_core::text::xml_escape_attr(&name));
        out_buf.push_str("\" generated=\"");
        out_buf.push_str(&now);
        out_buf.push_str("\">\n");
    }

    let mut exported = 0usize;
    for root_path in &paths {
        if signatures {
            // Code-skeleton view (reads chunks; works without summaries).
            let mut chunks = store.code_chunks_under(root_path, 0)?;
            if let Some(a) = &allow {
                chunks.retain(|c| a.contains(&c.entry_path));
            }
            if chunks.is_empty() {
                eprintln!("  \u{26a0} No indexed code under {root_path} matched — run `indexa deep {root_path}` first, or widen the slice.");
                continue;
            }
            out_buf.push_str(&indexa_query::render_signatures(
                &chunks,
                &format,
                !strip_comments,
            ));
            out_buf.push('\n');
            exported += 1;
            continue;
        }
        let tree = build_tree(&store, root_path, depth)?;
        let Some(tree) = tree else {
            eprintln!(
                "  \u{26a0} No summary for {root_path} \
                 — run `indexa summarize {root_path}` first."
            );
            continue;
        };
        // Apply the relational slice: prune to matched files + their ancestors; skip a path
        // with no match.
        let tree = match &allow {
            Some(a) => match prune_tree(tree, a) {
                Some(t) => t,
                None => continue,
            },
            None => tree,
        };
        let rendered = match format.as_str() {
            "md" | "markdown" => render_markdown(&tree),
            "json" => render_json(&tree),
            _ => render_xml(&tree, &now),
        };
        out_buf.push_str(&rendered);
        out_buf.push('\n');
        exported += 1;
    }

    // Optional importance-weights section (global; reuses the same renderer as `export`).
    if include_weights {
        out_buf.push_str(&indexa_query::render_weights(
            &store.list_weights(None).unwrap_or_default(),
            &format,
        ));
    }

    if is_xml {
        out_buf.push_str("</context>\n");
    }

    if exported == 0 {
        if allow.is_some() {
            bail!(
                "Nothing in pack \"{name}\" matched the slice (--changed-since / --category). \
                 Widen the window/category or drop the filter."
            );
        }
        let hint = if signatures {
            "have indexed code yet. Run `indexa deep <path>` first."
        } else {
            "have summaries yet. Run `indexa summarize <path>` or `indexa index <path>` first."
        };
        bail!("No paths in pack \"{name}\" {hint}");
    }

    finalize_export(
        out_buf,
        ExportSink {
            redact: !no_redact,
            token_budget,
            strict_budget,
            clipboard,
            output,
        },
    )
}

/// Format version for `pack export-def`/`pack import` JSON. Import refuses anything it doesn't
/// recognize (forward-safe), mirroring `snapshot.rs`'s `SNAPSHOT_VERSION` convention.
const PACK_DEF_VERSION: u32 = 1;

/// A pack's *definition* (name, description, member paths) — distinct from `pack export`'s
/// rendered *content* (summaries/chunks). Portable JSON: back it up, check it into a repo, or
/// hand it to a teammate to recreate the same pack on another machine/checkout.
#[derive(Serialize, Deserialize)]
struct PackDefinition {
    version: u32,
    name: String,
    description: Option<String>,
    generated_at: i64,
    paths: Vec<String>,
}

pub(crate) async fn cmd_pack_export_def(name: String, output: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    cmd_pack_export_def_at(&db_path, name, output)
}

/// `cmd_pack_export_def` with the DB path injected, so it's hermetically testable — mirrors the
/// `_at(db_path, …)` pattern used throughout this file (`cmd_pack_export_at`, `cmd_pack_refresh_at`
/// in later PRs).
fn cmd_pack_export_def_at(db_path: &Path, name: String, output: Option<String>) -> Result<()> {
    let store = Store::open(db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let paths = store.pack_paths(&pack.id)?;
    let def = PackDefinition {
        version: PACK_DEF_VERSION,
        name: pack.name,
        description: pack.description,
        generated_at: now_unix(),
        paths,
    };
    let json = serde_json::to_string_pretty(&def)?;
    if let Some(path) = output {
        std::fs::write(&path, &json).with_context(|| format!("writing {path}"))?;
        eprintln!("Wrote pack definition for \"{name}\" to {path}");
    } else {
        println!("{json}");
    }
    Ok(())
}

pub(crate) async fn cmd_pack_import(file: String, yes: bool) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    cmd_pack_import_at(&db_path, file, yes)
}

/// `cmd_pack_import` with the DB path injected, so it's hermetically testable.
///
/// Merge policy: an existing same-named pack requires `--yes` to proceed, and re-importing then
/// MERGES member paths into it (`add_pack_paths` is idempotent) rather than deleting and
/// recreating — destroying an existing pack just because a name collides would be needlessly
/// destructive, and would drop its current description if the import's is absent.
///
/// A pack definition is portable JSON that may be imported on a different machine/checkout where
/// the absolute member paths don't exist — each path is checked against the local disk; missing
/// ones are skipped with a warning rather than registered as dead pack members. This does NOT
/// reindex anything: it only restores the pack shell and membership. A path that's already
/// indexed on this machine is immediately searchable; an un-indexed one waits for a normal
/// `indexa index`/`pack refresh` pass, same scope separation as the rest of Context Packs.
fn cmd_pack_import_at(db_path: &Path, file: String, yes: bool) -> Result<()> {
    let raw = std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?;
    let def: PackDefinition = serde_json::from_str(&raw).context("parsing pack definition JSON")?;
    if def.version != PACK_DEF_VERSION {
        bail!(
            "pack definition version {} is not supported (expected {PACK_DEF_VERSION})",
            def.version
        );
    }
    let mut store = Store::open(db_path)?;
    let pack_id = match store.pack_by_name(&def.name)? {
        Some(_) if !yes => bail!(
            "a pack named \"{}\" already exists — pass --yes to merge into it",
            def.name
        ),
        Some(existing) => existing.id,
        None => store.create_pack(&def.name, def.description.as_deref())?,
    };
    let (present, missing): (Vec<String>, Vec<String>) =
        def.paths.into_iter().partition(|p| Path::new(p).exists());
    for p in &missing {
        println!("  ⚠ path not found on disk, skipping: {p}");
    }
    if !present.is_empty() {
        store.add_pack_paths(&pack_id, &present)?;
    }
    println!(
        "Imported pack \"{}\": {} path{} added, {} missing.",
        def.name,
        present.len(),
        if present.len() == 1 { "" } else { "s" },
        missing.len()
    );
    Ok(())
}

pub(crate) async fn cmd_pack_rename(name: String, new_name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    if store.pack_by_name(&new_name)?.is_some() {
        bail!("a pack named \"{new_name}\" already exists.");
    }
    store.rename_pack(&pack.id, &new_name)?;
    println!("Renamed pack \"{name}\" → \"{new_name}\".");
    Ok(())
}

pub(crate) async fn cmd_pack_delete(name: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let mut store = Store::open(&db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    store.delete_pack(&pack.id)?;
    println!("Deleted pack \"{name}\". (Indexed files are untouched.)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_def_round_trips_name_description_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.rs");
        let file_b = dir.path().join("b.rs");
        std::fs::write(&file_a, b"fn a() {}").unwrap();
        std::fs::write(&file_b, b"fn b() {}").unwrap();
        let a_s = file_a.to_string_lossy().to_string();
        let b_s = file_b.to_string_lossy().to_string();

        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store
            .create_pack("Auth", Some("Auth and session handling"))
            .unwrap();
        let pack = store.pack_by_name("Auth").unwrap().unwrap();
        store
            .add_pack_paths(&pack.id, &[a_s.clone(), b_s.clone()])
            .unwrap();
        drop(store);

        let out_path = dir.path().join("def.json");
        cmd_pack_export_def_at(
            &db_path,
            "Auth".to_string(),
            Some(out_path.to_string_lossy().to_string()),
        )
        .unwrap();

        let raw = std::fs::read_to_string(&out_path).unwrap();
        let def: PackDefinition = serde_json::from_str(&raw).unwrap();
        assert_eq!(def.version, PACK_DEF_VERSION);
        assert_eq!(def.name, "Auth");
        assert_eq!(
            def.description.as_deref(),
            Some("Auth and session handling")
        );
        assert_eq!(def.paths, vec![a_s, b_s]);
    }

    #[test]
    fn export_def_errors_on_unknown_pack() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idx.db");
        // Open once to create the DB file/schema, then close — no pack ever created.
        drop(Store::open(&db_path).unwrap());

        let err = cmd_pack_export_def_at(&db_path, "Ghost".to_string(), None).unwrap_err();
        assert!(err.to_string().contains("no pack named \"Ghost\""));
    }

    #[test]
    fn import_creates_new_pack_and_skips_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real_file = dir.path().join("real.rs");
        std::fs::write(&real_file, b"fn real() {}").unwrap();
        let real_s = real_file.to_string_lossy().to_string();
        let missing_s = dir.path().join("missing.rs").to_string_lossy().into_owned();

        let def = PackDefinition {
            version: PACK_DEF_VERSION,
            name: "Imported".to_string(),
            description: Some("from a teammate".to_string()),
            generated_at: 1,
            paths: vec![real_s.clone(), missing_s],
        };
        let def_path = dir.path().join("def.json");
        std::fs::write(&def_path, serde_json::to_string_pretty(&def).unwrap()).unwrap();

        let db_path = dir.path().join("idx.db");
        cmd_pack_import_at(&db_path, def_path.to_string_lossy().to_string(), false).unwrap();

        let store = Store::open(&db_path).unwrap();
        let pack = store.pack_by_name("Imported").unwrap().unwrap();
        assert_eq!(pack.description.as_deref(), Some("from a teammate"));
        let paths = store.pack_paths(&pack.id).unwrap();
        assert_eq!(
            paths,
            vec![real_s],
            "only the on-disk path should be registered"
        );
    }

    #[test]
    fn import_refuses_existing_pack_without_yes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store.create_pack("Auth", None).unwrap();
        drop(store);

        let def = PackDefinition {
            version: PACK_DEF_VERSION,
            name: "Auth".to_string(),
            description: None,
            generated_at: 1,
            paths: vec![],
        };
        let def_path = dir.path().join("def.json");
        std::fs::write(&def_path, serde_json::to_string_pretty(&def).unwrap()).unwrap();

        let err = cmd_pack_import_at(&db_path, def_path.to_string_lossy().to_string(), false)
            .unwrap_err();
        assert!(err.to_string().contains("--yes"));
    }

    #[test]
    fn import_merges_into_existing_pack_with_yes() {
        let dir = tempfile::tempdir().unwrap();
        let existing_file = dir.path().join("existing.rs");
        let new_file = dir.path().join("new.rs");
        std::fs::write(&existing_file, b"fn existing() {}").unwrap();
        std::fs::write(&new_file, b"fn new_fn() {}").unwrap();
        let existing_s = existing_file.to_string_lossy().to_string();
        let new_s = new_file.to_string_lossy().to_string();

        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        let pack_id = store.create_pack("Auth", None).unwrap();
        store
            .add_pack_paths(&pack_id, std::slice::from_ref(&existing_s))
            .unwrap();
        drop(store);

        let def = PackDefinition {
            version: PACK_DEF_VERSION,
            name: "Auth".to_string(),
            description: None,
            generated_at: 1,
            paths: vec![new_s.clone()],
        };
        let def_path = dir.path().join("def.json");
        std::fs::write(&def_path, serde_json::to_string_pretty(&def).unwrap()).unwrap();

        cmd_pack_import_at(&db_path, def_path.to_string_lossy().to_string(), true).unwrap();

        let store = Store::open(&db_path).unwrap();
        let pack = store.pack_by_name("Auth").unwrap().unwrap();
        let mut paths = store.pack_paths(&pack.id).unwrap();
        paths.sort();
        let mut expected = vec![existing_s, new_s];
        expected.sort();
        assert_eq!(
            paths, expected,
            "merge must keep the existing member AND add the new one"
        );
    }

    #[test]
    fn import_rejects_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idx.db");
        let def_path = dir.path().join("def.json");
        std::fs::write(
            &def_path,
            r#"{"version":999,"name":"Whatever","description":null,"generated_at":1,"paths":[]}"#,
        )
        .unwrap();

        let err = cmd_pack_import_at(&db_path, def_path.to_string_lossy().to_string(), false)
            .unwrap_err();
        assert!(err.to_string().contains("version"));
    }
}
