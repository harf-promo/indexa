use anyhow::{Context, Result};
use indexa_core::store::{EdgeRecord, Store, SummaryRecord};
use serde::{Deserialize, Serialize};

use super::helpers::require_index_db;

/// Snapshot format version. Import refuses anything it doesn't recognize (forward-safe).
const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    generated_at: i64,
    summaries: Vec<SummaryDto>,
    edges: Vec<EdgeDto>,
    weights: Vec<WeightDto>,
}

#[derive(Serialize, Deserialize)]
struct SummaryDto {
    path: String,
    kind: String,
    parent_path: Option<String>,
    depth: i64,
    summary: String,
    summary_l0: Option<String>,
    child_count: i64,
    byte_size: i64,
    model: String,
    source_hash: String,
    generated_at: i64,
}

#[derive(Serialize, Deserialize)]
struct EdgeDto {
    from: String,
    kind: String,
    to: String,
}

#[derive(Serialize, Deserialize)]
struct WeightDto {
    kind: String,
    target: String,
    weight: f32,
    reason: Option<String>,
}

/// `indexa snapshot export` — serialize the summary tree + call graph + importance
/// weights (the expensive-to-recompute AI layer) as a portable, versioned JSON document.
/// Excludes raw chunks/embeddings (bulky + model-specific), so it's for sharing the
/// *understanding* of an index, not its full searchable content.
pub(crate) async fn cmd_snapshot_export(output: Option<String>) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    let summaries = store.all_summaries()?;
    if summaries.is_empty() {
        anyhow::bail!("nothing to snapshot — no summaries. Run `indexa summarize` first.");
    }
    let snap = Snapshot {
        version: SNAPSHOT_VERSION,
        generated_at: now_unix(),
        summaries: summaries
            .into_iter()
            .map(|s| SummaryDto {
                path: s.path,
                kind: s.kind,
                parent_path: s.parent_path,
                depth: s.depth,
                summary: s.summary,
                summary_l0: s.summary_l0,
                child_count: s.child_count,
                byte_size: s.byte_size,
                model: s.model,
                source_hash: s.source_hash,
                generated_at: s.generated_at,
            })
            .collect(),
        edges: store
            .all_edges()?
            .into_iter()
            .map(|e| EdgeDto {
                from: e.from_path,
                kind: e.kind,
                to: e.to_ref,
            })
            .collect(),
        weights: store
            .list_weights(None)?
            .into_iter()
            .map(|w| WeightDto {
                kind: w.target_kind,
                target: w.target,
                weight: w.weight,
                reason: w.reason,
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&snap)?;
    if let Some(path) = output {
        std::fs::write(&path, &json).with_context(|| format!("writing snapshot to '{path}'"))?;
        eprintln!(
            "Wrote snapshot v{SNAPSHOT_VERSION} ({} summaries, {} edges, {} weights) to {path}.",
            snap.summaries.len(),
            snap.edges.len(),
            snap.weights.len()
        );
    } else {
        println!("{json}");
    }
    Ok(())
}

/// `indexa snapshot import <file>` — load a snapshot into the index. Refuses unless the
/// index has **no summaries** (an empty/fresh index), to avoid merge-conflict ambiguity:
/// the use case is reconstructing a shared index on a new machine, not merging.
pub(crate) async fn cmd_snapshot_import(path: String) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading snapshot '{path}'"))?;
    let snap: Snapshot = serde_json::from_str(&raw)
        .context("parsing snapshot JSON (is this an indexa snapshot?)")?;
    if snap.version != SNAPSHOT_VERSION {
        anyhow::bail!(
            "snapshot version {} is not supported (this build reads v{SNAPSHOT_VERSION}).",
            snap.version
        );
    }

    let mut store = Store::open(&db_path)?;
    if store.summary_count()? > 0 {
        anyhow::bail!(
            "import requires an index with no summaries (found existing summaries). \
             Use a fresh index — `indexa rm -r <root>` or a clean config/data dir — then re-import."
        );
    }

    for s in &snap.summaries {
        store.upsert_summary(&SummaryRecord {
            path: s.path.clone(),
            kind: s.kind.clone(),
            parent_path: s.parent_path.clone(),
            depth: s.depth,
            summary: s.summary.clone(),
            summary_l0: s.summary_l0.clone(),
            embedding: None,
            child_count: s.child_count,
            byte_size: s.byte_size,
            model: s.model.clone(),
            source_hash: s.source_hash.clone(),
            generated_at: s.generated_at,
        })?;
    }
    let edges: Vec<EdgeRecord> = snap
        .edges
        .iter()
        .map(|e| EdgeRecord {
            from_path: e.from.clone(),
            kind: e.kind.clone(),
            to_ref: e.to.clone(),
        })
        .collect();
    store.upsert_edges(&edges)?;
    for w in &snap.weights {
        store.set_weight(&w.kind, &w.target, w.weight, "user", w.reason.as_deref())?;
    }

    println!(
        "Imported snapshot v{}: {} summaries, {} edges, {} weights. \
         (Browse/export work; `ask`/`search` need a local `deep` — chunks aren't in snapshots.)",
        snap.version,
        snap.summaries.len(),
        snap.edges.len(),
        snap.weights.len()
    );
    Ok(())
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
