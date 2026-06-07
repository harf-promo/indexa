use anyhow::Result;
use indexa_core::{
    config::{Config, HybridMode},
    store::Store,
};
use serde::Serialize;

use super::helpers::{build_embedder, require_index_db};

#[derive(Serialize)]
struct HitJson {
    path: String,
    heading: String,
    seq: usize,
    score: f64,
    snippet: String,
}

/// `indexa search <query>` — fast content search over the index, returning ranked
/// chunk hits **without** LLM synthesis (that's what `ask` is for).
///
/// Defaults to sparse/BM25 so it works with no embedder / Ollama down — the robust
/// primitive. `--dense` / `--hybrid` opt into vector search (requires embeddings).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_search(
    query: String,
    top_k: Option<usize>,
    scope: Option<String>,
    dense: bool,
    hybrid: bool,
    json: bool,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    if store.chunk_count()? == 0 {
        if json {
            println!("[]");
        } else {
            println!("No deep-scanned content found. Run `indexa deep <path>` first.");
        }
        return Ok(());
    }

    // Sparse is the default (no embedder needed). --hybrid wins over --dense if both given.
    let mode = if hybrid {
        HybridMode::Rrf
    } else if dense {
        HybridMode::Dense
    } else {
        HybridMode::Sparse
    };
    let limit = top_k.unwrap_or(10);
    let scope = scope.as_deref().map(|s| shellexpand::tilde(s).into_owned());

    // Only embed when a vector arm is requested; fall back to sparse if it fails.
    let query_vec = if matches!(mode, HybridMode::Sparse) {
        None
    } else {
        match build_embedder(cfg, None) {
            Ok(embedder) => embedder.embed(&query).await.ok(),
            Err(_) => None,
        }
    };

    let hits = store.hybrid_search(
        &query,
        query_vec.as_deref(),
        &mode,
        scope.as_deref(),
        limit,
        cfg.retrieval.rrf_k as f32,
    )?;

    if json {
        let out: Vec<HitJson> = hits
            .iter()
            .map(|h| HitJson {
                path: h.entry_path.clone(),
                heading: h.heading.clone(),
                seq: h.seq,
                score: h.rrf_score,
                snippet: h.text.chars().take(160).collect(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("No results for \"{query}\".");
        return Ok(());
    }
    for (i, h) in hits.iter().enumerate() {
        let loc = if h.heading.is_empty() {
            h.entry_path.clone()
        } else {
            format!("{} — {}", h.entry_path, h.heading)
        };
        println!("{:>2}. [{:.4}] {}", i + 1, h.rrf_score, loc);
    }
    Ok(())
}
