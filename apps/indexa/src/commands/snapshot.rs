use anyhow::{Context, Result};
use indexa_core::store::{EdgeRecord, Store, SummaryRecord};
use indexa_query::redact::redact_secrets;
use serde::{Deserialize, Serialize};

use super::helpers::{now_unix, require_index_db};

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

/// Build the snapshot document from an open store, with secrets redacted from the AI-generated
/// summary text. Extracted from [`cmd_snapshot_export`] so the redaction contract is
/// unit-testable without going through the real `require_index_db` CLI path. Bails when the
/// index has no summaries.
fn build_snapshot(store: &Store) -> Result<Snapshot> {
    let summaries = store.all_summaries()?;
    if summaries.is_empty() {
        anyhow::bail!("nothing to snapshot — no summaries. Run `indexa summarize` first.");
    }
    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
        generated_at: now_unix(),
        summaries: summaries
            .into_iter()
            .map(|s| SummaryDto {
                path: s.path,
                kind: s.kind,
                parent_path: s.parent_path,
                depth: s.depth,
                // Redact secrets from the AI-generated summary text before it leaves the
                // machine — a snapshot is an export like packs/resources/whole-tree export
                // (which already redact), and summaries are derived from file content so they
                // can echo a committed key/token.
                summary: redact_secrets(&s.summary).0,
                summary_l0: s.summary_l0.as_deref().map(|t| redact_secrets(t).0),
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
    })
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
    let snap = build_snapshot(&store)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_export_redacts_secrets_in_summary_text() {
        // A summary is derived from file content, so it can echo a committed secret. The
        // exported snapshot must scrub it (same contract as pack/resource/whole-tree export) —
        // a snapshot is data meant to leave the machine. `summary` and `summary_l0` each get a
        // *different* secret shape so the assertions can't pass via just one field being
        // redacted while the other leaks — each field's redaction is proven independently.
        // AWS access-key-id shape, in `summary`.
        let aws_key = "AKIAIOSFODNN7EXAMPLE";
        // GitHub token shape, in `summary_l0`. Assembled from split literals at runtime (per
        // the convention in `redact.rs`'s tests) so the source never contains a contiguous
        // provider-shaped token — GitHub push protection blocks a commit that does.
        let gh_token = format!("ghp_{}", "0123456789abcdefABCDEF0123456789abcd");
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&SummaryRecord {
                path: "/proj/config.rs".into(),
                kind: "file".into(),
                parent_path: Some("/proj".into()),
                depth: 1,
                summary: format!("Sets up AWS auth with key {aws_key} and a client."),
                summary_l0: Some(format!("Auth setup (token {gh_token}).")),
                embedding: None,
                child_count: 0,
                byte_size: 100,
                model: "test".into(),
                source_hash: "h".into(),
                generated_at: 0,
            })
            .unwrap();

        let snap = build_snapshot(&store).unwrap();
        let json = serde_json::to_string_pretty(&snap).unwrap();
        assert!(
            !json.contains(aws_key),
            "raw AWS key leaked from `summary` into the exported snapshot JSON: {json}"
        );
        assert!(
            !json.contains(&gh_token),
            "raw GitHub token leaked from `summary_l0` into the exported snapshot JSON: {json}"
        );
        assert!(
            json.contains("[REDACTED-aws-key]"),
            "expected the AWS-key redaction marker: {json}"
        );
        assert!(
            json.contains("[REDACTED-github-token]"),
            "expected the GitHub-token redaction marker: {json}"
        );
        // Prose that isn't secret-shaped survives untouched.
        assert!(json.contains("Sets up AWS auth"));
        assert!(json.contains("Auth setup"));
        assert!(json.contains("/proj/config.rs"));
    }

    #[test]
    fn snapshot_export_bails_on_empty_index() {
        let store = Store::open_in_memory().unwrap();
        // `Snapshot` isn't `Debug`, so sidestep `unwrap_err` (which requires the `Ok` side to
        // be `Debug` for its panic message) via `Result::err`.
        let err = build_snapshot(&store).err().unwrap();
        assert!(err.to_string().contains("no summaries"));
    }
}
