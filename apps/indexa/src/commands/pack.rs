use anyhow::{bail, Result};
use indexa_core::{
    config::{Config, HybridMode},
    store::Store,
};
use indexa_query::{
    build_export_filter, build_tree, prune_tree, render_graph, render_graph_mermaid, render_json,
    render_markdown, render_xml,
};

use super::cmd_deep;
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
    // Freshness: indexed member files whose stored chunks are out of date with the disk
    // (edited without a rescan, or deleted). Best-effort — a stat failure must not fail the show.
    let stale = store.stale_pack_paths(&pack.id).unwrap_or_default();
    if !stale.is_empty() {
        println!(
            "\n{} indexed file{} stale — chunk content only, changed or removed since last \
             index — refresh with: indexa pack refresh \"{name}\"",
            stale.len(),
            if stale.len() == 1 { " is" } else { "s are" },
        );
    }
    Ok(())
}

/// Reindex a pack's stale members (files changed on disk since last indexed). Unlike the
/// fail-open freshness check `pack show`/`pack export` use, a `stale_pack_paths` error here
/// propagates — refresh's whole job is to report accurately, so silently showing 0 would mislead.
pub(crate) async fn cmd_pack_refresh(name: String, cfg: &Config) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    cmd_pack_refresh_at(&db_path, name, cfg).await
}

/// `cmd_pack_refresh` with the DB path injected (rather than resolved via `require_index_db`),
/// so it's hermetically testable — mirrors `resolve_target_roots_in`'s testability pattern.
///
/// Design decision (flag-for-review, not auto-remove): a stale member whose file has vanished
/// from disk entirely is left in `pack_paths` rather than silently dropped — mirroring
/// `decisions::expire_vanished_decisions`'s expire-don't-delete precedent for the review ledger.
/// It stays flagged (still reported by `stale_pack_paths` on the next call) until the user
/// explicitly runs `indexa pack remove`. This is safe to do unconditionally because `walk()`
/// (crates/core/src/walker.rs) is fail-open on an unreadable/missing root — `Err(_) =>
/// WalkState::Continue` — so handing `cmd_deep` a path that no longer exists just yields zero
/// entries for that root instead of aborting the whole reindex.
///
/// Chunk-level only, same caveat as `stale_pack_paths`: this brings chunk content current but
/// does not touch prose summaries — if a summary also needs updating, a separate `indexa
/// summarize <path>` is still required.
pub(crate) async fn cmd_pack_refresh_at(
    db_path: &std::path::Path,
    name: String,
    cfg: &Config,
) -> Result<()> {
    let mut store = Store::open(db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let stale = store.stale_pack_paths(&pack.id)?;
    if stale.is_empty() {
        println!("Pack \"{name}\" has no stale files.");
        return Ok(());
    }

    // Partition into what's still on disk (reindexable) vs. genuinely gone (flagged, not
    // removed — see the design-decision doc comment above).
    let (existing, missing): (Vec<String>, Vec<String>) = stale
        .iter()
        .cloned()
        .partition(|p| std::path::Path::new(p).exists());

    if !missing.is_empty() {
        println!(
            "{} member file{} no longer exist on disk — left flagged in the pack \
             (remove with: indexa pack remove \"{name}\" <path>):",
            missing.len(),
            if missing.len() == 1 { "" } else { "s" }
        );
        for m in &missing {
            println!("  \u{26a0} {m}");
        }
    }

    let detail = format!(
        "{} reindexed, {} vanished (left flagged)",
        existing.len(),
        missing.len()
    );
    if let Err(e) = store.record_pack_refreshed(&pack.id, &detail) {
        tracing::debug!("pack event (refreshed) not recorded: {e:#}");
    }

    if existing.is_empty() {
        println!("Nothing left to reindex in pack \"{name}\".");
        return Ok(());
    }

    println!(
        "Refreshing {} stale file{} in pack \"{name}\"…",
        existing.len(),
        if existing.len() == 1 { "" } else { "s" }
    );
    // Release the connection before `cmd_deep` opens its own.
    drop(store);
    // Each stale path is passed as its own root: `cmd_deep`/`walk()` (ignore::WalkBuilder) already
    // handle a bare file root correctly, so this reindexes exactly the stale files — no rescan of
    // the rest of the pack. `mode: None` defers to the configured `[describer] mode` (same as
    // every other caller that has no dedicated `--mode` flag of its own — forcing a mode here
    // would silently override the user's config). `no_embed: false` so embeddings are actually
    // brought current — that's the whole point of refresh. Folder-rollup summaries still need a
    // follow-up `indexa summarize`/worker pass, exactly as the hint text above and `pack show`'s
    // says (chunk-level only, see this function's doc comment).
    cmd_deep(existing, None, false, None, false, false, false, cfg).await?;
    println!(
        "Pack \"{name}\" refreshed (chunk index only — if a summary/description also needs \
         updating, run `indexa summarize <path>`)."
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_pack_export(
    name: String,
    format: String,
    output: Option<String>,
    out: Option<String>,
    depth: Option<usize>,
    include_weights: bool,
    include_graph: bool,
    graph_format: String,
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
    cmd_pack_export_at(
        &db_path,
        name,
        format,
        output,
        out,
        depth,
        include_weights,
        include_graph,
        graph_format,
        signatures,
        token_budget,
        strict_budget,
        clipboard,
        strip_comments,
        no_redact,
        changed_since,
        category,
    )
    .await
}

/// `cmd_pack_export` with the DB path injected, so it's hermetically testable — mirrors
/// `resolve_target_roots_in`'s testability pattern.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_pack_export_at(
    db_path: &std::path::Path,
    name: String,
    format: String,
    output: Option<String>,
    out: Option<String>,
    depth: Option<usize>,
    include_weights: bool,
    include_graph: bool,
    graph_format: String,
    signatures: bool,
    token_budget: Option<usize>,
    strict_budget: bool,
    clipboard: bool,
    strip_comments: bool,
    no_redact: bool,
    changed_since: Option<String>,
    category: Option<String>,
) -> Result<()> {
    let mut store = Store::open(db_path)?;
    let pack = store
        .pack_by_name(&name)?
        .ok_or_else(|| anyhow::anyhow!("no pack named \"{name}\""))?;
    let paths = store.pack_paths(&pack.id)?;
    if paths.is_empty() {
        bail!("Pack \"{name}\" has no paths. Add paths first with `indexa pack add`.");
    }

    // 4.2 — OKF bundle export is a directory of files, not a single blob: handled entirely
    // separately from the xml/md/json rendering path below (no --output/--clipboard/
    // --token-budget — those are single-artifact concepts that don't apply to a bundle).
    if format == "okf" {
        let Some(out_dir) = out else {
            bail!("--format okf requires --out <dir> (a directory to write the bundle into).");
        };
        let out_dir = expand(&out_dir);
        let mut roots = Vec::new();
        for root_path in &paths {
            match build_tree(&store, root_path, depth)? {
                Some(tree) => roots.push(tree),
                None => eprintln!(
                    "  \u{26a0} No summary for {root_path} — run `indexa summarize {root_path}` first."
                ),
            }
        }
        if roots.is_empty() {
            bail!("No paths in pack \"{name}\" have summaries yet. Run `indexa summarize <path>` or `indexa index <path>` first.");
        }
        let events: Vec<(String, Option<String>, i64)> = store
            .pack_events(&pack.id)?
            .into_iter()
            .map(|e| (e.event, e.detail, e.at))
            .collect();
        let bundle = indexa_query::render_okf_bundle(&name, &roots, &events);

        std::fs::create_dir_all(&out_dir)
            .map_err(|e| anyhow::anyhow!("cannot create {out_dir}: {e}"))?;
        for (rel_path, content) in &bundle {
            let content = if no_redact {
                content.clone()
            } else {
                indexa_query::redact::redact_secrets(content).0
            };
            let file_path = std::path::Path::new(&out_dir).join(rel_path);
            std::fs::write(&file_path, content)
                .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", file_path.display()))?;
        }
        println!(
            "Wrote OKF v0.1 bundle for pack \"{name}\" to {out_dir} ({} file(s): index.md, log.md, {} concept file(s)).",
            bundle.len(),
            bundle.len() - 2
        );
        if let Err(e) = store.record_pack_exported(&pack.id, "okf") {
            tracing::debug!("pack event (exported) not recorded: {e:#}");
        }
        return Ok(());
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
        // Freshness (G2b): same best-effort stat check `pack show` surfaces (a stat error must
        // not fail an export that worked before) — confined to the XML path so md/json exports
        // (which have no wrapping header to hang the attribute off) don't pay the stat sweep.
        let stale_count = store.stale_pack_paths(&pack.id).unwrap_or_default().len();
        out_buf.push_str("<context pack=\"");
        out_buf.push_str(&indexa_core::text::xml_escape_attr(&name));
        out_buf.push_str("\" generated=\"");
        out_buf.push_str(&now);
        out_buf.push_str("\" stale_files=\"");
        out_buf.push_str(&stale_count.to_string());
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

    // Optional call-graph section (1.6): a single pack path scopes the graph to it; multiple
    // paths fall back to the whole-index scope ("/"), same precedent as `indexa export
    // --include-graph`. Capped at 200 edges (the MCP `code_graph` tool's default).
    if include_graph {
        let scope = if paths.len() == 1 {
            paths[0].clone()
        } else {
            "/".to_owned()
        };
        if let Ok(graph) = store.code_graph(&scope, 200, false) {
            let section = if graph_format == "mermaid" {
                render_graph_mermaid(&graph)
            } else {
                render_graph(&graph, &format)
            };
            out_buf.push_str(&section);
        }
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
    )?;
    // Record the event only once the artifact is actually written (4.1) — a failed
    // finalize (disk error, budget overflow) must not log a phantom export.
    if let Err(e) = store.record_pack_exported(&pack.id, &format) {
        tracing::debug!("pack event (exported) not recorded: {e:#}");
    }
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

/// `indexa pack import <bundle_dir> [--force] [--name <override>]` (4.3) — reconstructs a
/// pack from a 4.2 OKF export's `manifest.json`. Same-machine / machine-migration scope,
/// NOT team sharing: every path must already exist in THIS machine's index (no remote
/// fetching, no re-creating entries), and must be under an indexed root — those two checks
/// are unconditional (never bypassed by `--force`). `--force` overrides only the checksum
/// conflict check: an item whose current `source_hash` no longer matches the manifest's
/// recorded hash (the underlying file changed since export) is skipped by default (fail
/// fast, per the plan) and imported anyway with `--force`. Tolerant-consumer: unrecognized
/// manifest.json fields are silently ignored (`PackManifest`'s `#[serde(default)]` fields +
/// serde_json's default non-`deny_unknown_fields` behavior) rather than rejected.
/// The outcome of validating a manifest's items against the live index (4.3) — pure and
/// store-only (no filesystem/CLI I/O), so it's testable against an in-memory `Store` without
/// touching the real user data directory. `valid` is what actually gets imported;
/// `hard_skips` (missing from the index / outside an indexed root) are excluded
/// unconditionally; `hash_conflicts` (content changed since export) are excluded unless
/// `force`, in which case they're included in `valid` too (and also listed, so the caller
/// can report which ones drifted). Only `ManifestItem`s with `is_root` are considered at
/// all — a bundle's manifest also lists every DESCENDANT reached by walking a root's summary
/// tree (so the bundle's own concept files/hashes are complete), but `pack_paths` only ever
/// held the pack's original root anchors; re-adding every descendant too would give the
/// reconstructed pack a different (more granular) shape than the original, and a later
/// export would then double-list descendants (once as their own top-level tree, once as a
/// child of the root's tree) — caught by a real export→import→export round-trip, not by any
/// unit test, since the unit tests only checked ONE side of the round-trip in isolation.
struct ImportPlan {
    valid: Vec<String>,
    hard_skips: Vec<String>,
    hash_conflicts: Vec<String>,
}

fn resolve_import_plan(
    store: &Store,
    manifest: &indexa_query::PackManifest,
    force: bool,
) -> Result<ImportPlan> {
    let roots = store.root_paths()?;
    let under_a_root = |p: &str| {
        roots
            .iter()
            .any(|r| p == r || p.starts_with(&format!("{}/", r.trim_end_matches('/'))))
    };

    let mut valid: Vec<String> = Vec::new();
    let mut hard_skips: Vec<String> = Vec::new();
    let mut hash_conflicts: Vec<String> = Vec::new();
    for item in manifest.items.iter().filter(|i| i.is_root) {
        if store.entry_by_path(&item.path)?.is_none() {
            hard_skips.push(format!("{} — not found in the index", item.path));
            continue;
        }
        if !under_a_root(&item.path) {
            hard_skips.push(format!("{} — not under any indexed root", item.path));
            continue;
        }
        let current_hash = store
            .summary_by_path(&item.path)?
            .map(|s| s.source_hash)
            .unwrap_or_default();
        if current_hash != item.hash {
            hash_conflicts.push(format!(
                "{} — content changed since export (manifest hash {}, current {})",
                item.path,
                item.hash,
                if current_hash.is_empty() {
                    "<no summary yet>".to_owned()
                } else {
                    current_hash
                }
            ));
            if !force {
                continue;
            }
        }
        valid.push(item.path.clone());
    }

    Ok(ImportPlan {
        valid,
        hard_skips,
        hash_conflicts,
    })
}

pub(crate) async fn cmd_pack_import(
    bundle_dir: String,
    force: bool,
    name: Option<String>,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let bundle_dir = expand(&bundle_dir);
    let manifest_path = std::path::Path::new(&bundle_dir).join("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: indexa_query::PackManifest =
        serde_json::from_str(&manifest_text).map_err(|e| {
            anyhow::anyhow!(
                "{} is not a valid pack manifest: {e}",
                manifest_path.display()
            )
        })?;
    if manifest.items.is_empty() {
        bail!(
            "{} has no items — nothing to import.",
            manifest_path.display()
        );
    }

    let mut store = Store::open(&db_path)?;
    let ImportPlan {
        valid,
        hard_skips,
        hash_conflicts,
    } = resolve_import_plan(&store, &manifest, force)?;

    if !hash_conflicts.is_empty() && !force {
        eprintln!(
            "Cannot import — {} item(s) changed since export:",
            hash_conflicts.len()
        );
        for c in &hash_conflicts {
            eprintln!("  \u{26a0} {c}");
        }
        bail!(
            "Re-export the bundle from the current index, or pass --force to import anyway \
             (accepting the drifted content)."
        );
    }
    if valid.is_empty() {
        bail!(
            "No importable paths — every item in {} is missing from the index or outside an \
             indexed root.",
            manifest_path.display()
        );
    }

    let pack_name = name.unwrap_or_else(|| manifest.name.clone());
    if pack_name.trim().is_empty() {
        bail!("manifest.json has no pack name — pass one explicitly with --name.");
    }
    if store.pack_by_name(&pack_name)?.is_some() {
        bail!(
            "a pack named \"{pack_name}\" already exists — choose a different --name, or \
             rename/delete the existing pack first."
        );
    }

    let pack_id = store.create_pack(&pack_name, manifest.description.as_deref())?;
    store.add_pack_paths(&pack_id, &valid)?;

    println!(
        "Imported pack \"{pack_name}\" with {} path(s) from {bundle_dir}.",
        valid.len()
    );
    if !hard_skips.is_empty() {
        println!(
            "{} path(s) skipped (not indexed / outside indexed roots):",
            hard_skips.len()
        );
        for s in &hard_skips {
            println!("  \u{26a0} {s}");
        }
    }
    if force && !hash_conflicts.is_empty() {
        println!(
            "{} path(s) imported despite a content-hash mismatch since export (--force):",
            hash_conflicts.len()
        );
        for c in &hash_conflicts {
            println!("  \u{26a0} {c}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::store::SummaryRecord;
    use indexa_core::walker::{Entry, EntryKind};
    use indexa_query::ManifestItem;

    fn seed_indexed_file(store: &mut Store, root: &str, path: &str, hash: &str) {
        store
            .upsert_entries(&[
                Entry {
                    path: std::path::PathBuf::from(root),
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                },
                Entry {
                    path: std::path::PathBuf::from(path),
                    kind: EntryKind::File,
                    size: 10,
                    modified: None,
                    hint: None,
                    is_binary: false,
                },
            ])
            .unwrap();
        store
            .upsert_summary(&SummaryRecord {
                path: path.to_owned(),
                kind: "file".to_owned(),
                parent_path: Some(root.to_owned()),
                depth: 1,
                summary: "A summary.".to_owned(),
                summary_l0: None,
                embedding: None,
                child_count: 0,
                byte_size: 10,
                model: "test".to_owned(),
                source_hash: hash.to_owned(),
                generated_at: 0,
            })
            .unwrap();
    }

    fn manifest_with(items: Vec<(&str, &str)>) -> indexa_query::PackManifest {
        indexa_query::PackManifest {
            pack_format_version: "0.1".to_owned(),
            name: "proj".to_owned(),
            description: None,
            items: items
                .into_iter()
                .map(|(path, hash)| ManifestItem {
                    path: path.to_owned(),
                    hash: hash.to_owned(),
                    filename: "x.md".to_owned(),
                    is_root: true,
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_import_plan_accepts_a_matching_indexed_file() {
        let mut store = Store::open_in_memory().unwrap();
        seed_indexed_file(&mut store, "/root", "/root/a.rs", "hash1");
        let manifest = manifest_with(vec![("/root/a.rs", "hash1")]);
        let plan = resolve_import_plan(&store, &manifest, false).unwrap();
        assert_eq!(plan.valid, vec!["/root/a.rs".to_owned()]);
        assert!(plan.hard_skips.is_empty());
        assert!(plan.hash_conflicts.is_empty());
    }

    #[test]
    fn resolve_import_plan_hard_skips_a_path_not_in_the_index() {
        let store = Store::open_in_memory().unwrap();
        let manifest = manifest_with(vec![("/nope.rs", "hash1")]);
        let plan = resolve_import_plan(&store, &manifest, false).unwrap();
        assert!(plan.valid.is_empty());
        assert_eq!(plan.hard_skips.len(), 1);
        assert!(plan.hard_skips[0].contains("not found in the index"));
    }

    #[test]
    fn resolve_import_plan_hard_skips_a_path_outside_any_indexed_root() {
        let mut store = Store::open_in_memory().unwrap();
        // Indexed under /root, but the manifest references a path under a sibling that
        // merely shares a string prefix — must not be treated as "under a root".
        seed_indexed_file(&mut store, "/root", "/root/a.rs", "hash1");
        store
            .upsert_entries(&[Entry {
                path: std::path::PathBuf::from("/rootother/b.rs"),
                kind: EntryKind::File,
                size: 1,
                modified: None,
                hint: None,
                is_binary: false,
            }])
            .unwrap();
        let manifest = manifest_with(vec![("/rootother/b.rs", "whatever")]);
        let plan = resolve_import_plan(&store, &manifest, false).unwrap();
        assert!(plan.valid.is_empty());
        assert_eq!(plan.hard_skips.len(), 1);
        assert!(plan.hard_skips[0].contains("not under any indexed root"));
    }

    #[test]
    fn resolve_import_plan_flags_a_hash_conflict_and_excludes_it_without_force() {
        let mut store = Store::open_in_memory().unwrap();
        seed_indexed_file(&mut store, "/root", "/root/a.rs", "current-hash");
        let manifest = manifest_with(vec![("/root/a.rs", "old-hash")]);

        let without_force = resolve_import_plan(&store, &manifest, false).unwrap();
        assert!(without_force.valid.is_empty());
        assert_eq!(without_force.hash_conflicts.len(), 1);

        let with_force = resolve_import_plan(&store, &manifest, true).unwrap();
        assert_eq!(with_force.valid, vec!["/root/a.rs".to_owned()]);
        assert_eq!(
            with_force.hash_conflicts.len(),
            1,
            "force still reports the drift, just doesn't exclude the item"
        );
    }

    #[test]
    fn resolve_import_plan_handles_a_mix_of_valid_and_problem_items() {
        let mut store = Store::open_in_memory().unwrap();
        seed_indexed_file(&mut store, "/root", "/root/a.rs", "hash1");
        seed_indexed_file(&mut store, "/root", "/root/b.rs", "hash2");
        let manifest = manifest_with(vec![
            ("/root/a.rs", "hash1"),  // valid
            ("/root/b.rs", "stale"),  // hash conflict
            ("/root/gone.rs", "any"), // not indexed
        ]);
        let plan = resolve_import_plan(&store, &manifest, false).unwrap();
        assert_eq!(plan.valid, vec!["/root/a.rs".to_owned()]);
        assert_eq!(plan.hash_conflicts.len(), 1);
        assert_eq!(plan.hard_skips.len(), 1);
    }

    /// Regression test for a real round-trip bug: a bundle's manifest lists every summarized
    /// descendant (so the bundle's own concept files are complete), not just the pack's
    /// original root paths — importing every listed item as a pack_path would reconstruct a
    /// pack with a different (more granular) shape than the original.
    #[test]
    fn resolve_import_plan_only_imports_root_items_not_descendants() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                Entry {
                    path: std::path::PathBuf::from("/root"),
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                },
                Entry {
                    path: std::path::PathBuf::from("/root/child.rs"),
                    kind: EntryKind::File,
                    size: 10,
                    modified: None,
                    hint: None,
                    is_binary: false,
                },
            ])
            .unwrap();
        store
            .upsert_summary(&SummaryRecord {
                path: "/root".to_owned(),
                kind: "dir".to_owned(),
                parent_path: None,
                depth: 0,
                summary: "A root.".to_owned(),
                summary_l0: None,
                embedding: None,
                child_count: 1,
                byte_size: 0,
                model: "test".to_owned(),
                source_hash: "roothash".to_owned(),
                generated_at: 0,
            })
            .unwrap();
        store
            .upsert_summary(&SummaryRecord {
                path: "/root/child.rs".to_owned(),
                kind: "file".to_owned(),
                parent_path: Some("/root".to_owned()),
                depth: 1,
                summary: "A child.".to_owned(),
                summary_l0: None,
                embedding: None,
                child_count: 0,
                byte_size: 10,
                model: "test".to_owned(),
                source_hash: "childhash".to_owned(),
                generated_at: 0,
            })
            .unwrap();
        let manifest = indexa_query::PackManifest {
            pack_format_version: "0.1".to_owned(),
            name: "proj".to_owned(),
            description: None,
            items: vec![
                ManifestItem {
                    path: "/root".to_owned(),
                    hash: "roothash".to_owned(),
                    filename: "root.md".to_owned(),
                    is_root: true,
                },
                ManifestItem {
                    path: "/root/child.rs".to_owned(),
                    hash: "childhash".to_owned(),
                    filename: "child.md".to_owned(),
                    is_root: false,
                },
            ],
        };
        let plan = resolve_import_plan(&store, &manifest, false).unwrap();
        assert_eq!(
            plan.valid,
            vec!["/root".to_owned()],
            "only the is_root item must be imported as a pack_path"
        );
    }

    // ── G2a/G2b: staleness + refresh + export stale_files ───────────────────────

    use indexa_core::store::ChunkRecord;

    /// A chunk with an embedding present (so it counts toward `chunks_current_for_mtime`) and a
    /// non-null `language` (so `code_chunks_under`, the `--signatures` export path, picks it up).
    fn dummy_chunk_embedded(path: &str, text: &str) -> ChunkRecord {
        ChunkRecord {
            entry_path: path.to_owned(),
            seq: 0,
            heading: String::new(),
            text: text.to_owned(),
            language: Some("rust".to_owned()),
            embedding: Some(vec![0.1, 0.2, 0.3]),
            embed_model: Some("test".to_owned()),
            content_hash: None,
        }
    }

    #[tokio::test]
    async fn export_header_reports_stale_files_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, b"fn foo() {}").unwrap();
        let file_s = file.to_string_lossy().to_string();

        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store
            .upsert_chunks(&[dummy_chunk_embedded(&file_s, "fn foo() {}")])
            .unwrap();
        // Pin indexed_at to the epoch — long before the file's real mtime — so it reads stale
        // (mirrors `stale_pack_paths_flags_out_of_date_and_missing_members` in indexa-core).
        store
            .db_connection()
            .execute(
                "UPDATE chunks SET indexed_at = 1 WHERE entry_path = ?1",
                rusqlite::params![file_s],
            )
            .unwrap();
        let pack_id = store.create_pack("code", None).unwrap();
        store
            .add_pack_paths(&pack_id, std::slice::from_ref(&file_s))
            .unwrap();
        drop(store);

        let out_path = dir.path().join("out.xml");
        cmd_pack_export_at(
            &db_path,
            "code".to_string(),
            "xml".to_string(),
            Some(out_path.to_string_lossy().to_string()),
            None,
            None,
            false,
            false,
            "text".to_string(),
            true, // signatures — reads chunks directly, no summary fixture needed
            None,
            false,
            false,
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap();

        let xml = std::fs::read_to_string(&out_path).unwrap();
        assert!(
            xml.contains("stale_files=\"1\""),
            "expected stale_files=\"1\" in the export header, got: {xml}"
        );
    }

    #[tokio::test]
    async fn refresh_reports_no_stale_files_without_reindexing() {
        // A pack whose only member is current (fresh mtime, freshly-indexed chunk) has nothing
        // stale — refresh must report that and return WITHOUT calling `cmd_deep` (which would
        // need a reachable Ollama in a real run).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("b.rs");
        std::fs::write(&file, b"fn bar() {}").unwrap();
        let file_s = file.to_string_lossy().to_string();

        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store
            .upsert_chunks(&[dummy_chunk_embedded(&file_s, "fn bar() {}")])
            .unwrap();
        // Pin indexed_at far in the future — current relative to the file's real mtime.
        store
            .db_connection()
            .execute(
                "UPDATE chunks SET indexed_at = 4102444800 WHERE entry_path = ?1",
                rusqlite::params![file_s],
            )
            .unwrap();
        let pack_id = store.create_pack("clean", None).unwrap();
        store
            .add_pack_paths(&pack_id, std::slice::from_ref(&file_s))
            .unwrap();
        drop(store);

        let cfg = Config::default();
        // No stale files ⇒ returns Ok before ever touching `cmd_deep`/Ollama — this call would
        // hang/fail on the preflight if the "no stale" early return didn't fire first.
        cmd_pack_refresh_at(&db_path, "clean".to_string(), &cfg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_flags_a_vanished_member_without_removing_it_from_the_pack() {
        // The only stale member has been deleted from disk entirely — `existing` (reindexable)
        // is empty, so refresh must hit the "nothing left to reindex" early return WITHOUT
        // calling `cmd_deep`/Ollama, AND must leave the path in `pack_paths` (flag, don't
        // auto-remove — mirrors `decisions::expire_vanished_decisions`).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gone.rs");
        std::fs::write(&file, b"fn gone() {}").unwrap();
        let file_s = file.to_string_lossy().to_string();

        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store
            .upsert_chunks(&[dummy_chunk_embedded(&file_s, "fn gone() {}")])
            .unwrap();
        let pack_id = store.create_pack("vanish", None).unwrap();
        store
            .add_pack_paths(&pack_id, std::slice::from_ref(&file_s))
            .unwrap();
        drop(store);

        // Delete the file AFTER it was indexed — it can no longer be stat'd, so
        // `stale_pack_paths` flags it and `refresh` cannot reindex it.
        std::fs::remove_file(&file).unwrap();

        let cfg = Config::default();
        cmd_pack_refresh_at(&db_path, "vanish".to_string(), &cfg)
            .await
            .unwrap();

        let store = Store::open(&db_path).unwrap();
        let pack = store.pack_by_name("vanish").unwrap().unwrap();
        assert_eq!(
            store.pack_paths(&pack.id).unwrap(),
            vec![file_s.clone()],
            "a vanished member must stay in the pack, not be silently removed"
        );
        assert_eq!(
            store.stale_pack_paths(&pack.id).unwrap(),
            vec![file_s],
            "it must still read as stale on the next check"
        );
        let events = store.pack_events(&pack.id).unwrap();
        assert_eq!(
            events.last().unwrap().event,
            "refreshed",
            "the refresh attempt is recorded even though nothing was reindexed"
        );
        assert!(events
            .last()
            .unwrap()
            .detail
            .as_deref()
            .unwrap()
            .contains("1 vanished"));
    }
}
