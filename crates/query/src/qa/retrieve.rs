//! Retrieval + score adjustment: hybrid search, summary/weight/recency boosts, the
//! archive penalty, the code-intent boost, MMR, plus broad-question detection and the
//! project-overview composer. The synchronous core ([`retrieve`]) keeps the `&Store`
//! off any `.await` so the answer futures stay `Send`.

use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{AnnIndex, SearchHit, Store};

use super::mmr::apply_mmr;
use super::QaConfig;

/// Synchronous retrieval: hybrid search + summary boost. Kept separate so the
/// async orchestrator ([`answer`](super::answer)) can scope the `&Store` borrow to a block that
/// never spans an `.await` — keeping the resulting future `Send` (required by the
/// axum web server and the rmcp MCP server). `query_vec` is `None` for sparse-only.
pub(crate) fn retrieve(
    store: &Store,
    question: &str,
    query_vec: Option<&[f32]>,
    cfg: &QaConfig,
    ann: Option<&AnnIndex>,
) -> Result<Vec<SearchHit>> {
    let mut hits = store.hybrid_search_with_ann(
        question,
        query_vec,
        &cfg.mode,
        cfg.scope.as_deref(),
        cfg.top_k,
        cfg.rrf_k,
        ann,
    )?;
    // Belt-and-suspenders: drop any content-free stub chunk that slipped past the SQL filter
    // (e.g. the ANN dense arm returns ids straight from the HNSW index without running it),
    // so a "File: icon.png" placeholder can never surface as an answer source.
    hits.retain(|h| !indexa_core::store::is_stub_chunk(&h.text));
    if let Some(qvec) = query_vec {
        let _ = store.boost_with_summaries(
            &mut hits,
            qvec,
            cfg.summary_weight,
            cfg.summary_depth_alpha,
        );
    }
    // v0.8: apply per-file/dir/category importance weight boosts (multiplicative).
    if cfg.use_weights && !hits.is_empty() {
        let _ = store.boost_with_weights(&mut hits);
    }
    // v0.29: deprioritize archived/historical content so a stale doc (e.g. an old
    // known-issues file under docs/archive/ claiming an ancient version) can't dominate
    // an answer about the current state. Multiplicative, not exclusion — such docs stay
    // findable when the query is explicitly scoped into the historical path.
    apply_archive_penalty(&mut hits, cfg.scope.as_deref());
    // v0.39: when the question is about *implementation* ("which function implements…",
    // "the code that…", or it names a snake_case/CamelCase identifier), boost code-file
    // hits so the implementation outranks prose docs. Fixes the doc-bias where "how does X
    // work? which function?" returned only README/CHANGELOG and couldn't name the code.
    // Always-on like the archive penalty; inert on non-code questions and doc-only indexes.
    apply_code_intent_boost(&mut hits, question);
    // v0.31: optional recency boost — push recently-modified files up (the positive twin of the
    // archive penalty). Opt-in so it never silently re-ranks; uses mtime, not git.
    if cfg.use_recency_weight && !hits.is_empty() {
        let _ = store.boost_with_recency(&mut hits, cfg.recency_days);
    }
    // Re-sort after any score adjustment (idempotent when nothing changed — hybrid_search
    // already returns rrf-ordered hits).
    hits.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // v0.42: MMR (Maximal Marginal Relevance) diversity re-ranking.
    // Applied after all boosts so the penalty operates on the final relevance
    // scores. Skipped when lambda >= 1.0 (pure relevance / disabled) or when
    // the mode is sparse-only (no embeddings stored per chunk). Fails open:
    // any error fetching embeddings leaves the original order intact.
    //
    // v0.44: for code-intent questions ("which function implements…", or a bare
    // identifier) the user wants the implementing file's chunks, not topical
    // diversity — diversifying can drop the very chunk that names the answer.
    // Bias toward relevance (≥0.8) so same-file detail survives the diversity pass.
    // Clamp to [0,1]: a hand-edited out-of-range `[retrieval] mmr_lambda` (e.g. negative)
    // would otherwise make `(1 - lambda) > 1` and invert relevance in `mmr_score`. 1.0 stays
    // a no-op (the early-return below); 0.0 is pure diversity.
    let mmr_lambda = if is_code_intent(question) {
        cfg.mmr_lambda.max(0.8)
    } else {
        cfg.mmr_lambda
    }
    .clamp(0.0, 1.0);
    if mmr_lambda < 1.0 && !hits.is_empty() && !matches!(cfg.mode, HybridMode::Sparse) {
        let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
        match store.embeddings_for_chunks(&ids) {
            Ok(embeddings) if !embeddings.is_empty() => {
                hits = apply_mmr(hits, &embeddings, mmr_lambda);
            }
            Ok(_) => {
                // No embeddings stored for these chunks (index never had deep embeddings);
                // keep the relevance-sorted order.
            }
            Err(e) => {
                // Fail open: log and preserve existing order so ask never errors
                // due to MMR plumbing.
                tracing::warn!(
                    mmr_lambda,
                    "MMR embedding fetch failed, skipping diversity re-ranking: {e:#}"
                );
            }
        }
    }
    Ok(hits)
}

/// Full path segments that mark content as historical/superseded. Matched case-insensitively
/// and segment-bounded (so `archive` matches `…/docs/archive/x` but not `archived_data.rs`).
const HISTORICAL_SEGMENTS: [&str; 5] = ["archive", "archived", "historical", "deprecated", "old"];

/// How hard to push historical hits down. 0.15 keeps them retrievable (and explicitly
/// scopeable) but lets any current doc with a comparable raw score outrank them.
const ARCHIVE_PENALTY: f64 = 0.15;

// ── Broad-question (project-overview) detection ───────────────────────────────

/// Phrases that signal a question is asking for a project-level / thematic overview.
/// Conservative/false-negative-biased: phrase-level substrings only, never single common
/// words like "what" or "summary". A missed broad question still gets the small root-L0
/// fallback; the harm of a false-positive (enlarging the overview block for a specific
/// question) is minor — chunks still fill the remainder of the budget.
const BROAD_INTENT_TERMS: [&str; 20] = [
    "what is this project",
    "what's this project",
    "what is this repo",
    "what's this repo",
    "what is this about",
    "what's this about",
    "what does this project",
    "tell me about this project",
    "summarize this project",
    "summarise this project",
    "summary of the project",
    "summarize the project",
    "summarise the project",
    "main themes",
    "overall",
    "high level",
    "high-level",
    "what are these documents about",
    "across these documents",
    "the whole project",
];

pub fn is_broad_intent(question: &str) -> bool {
    let q = question.to_ascii_lowercase();
    BROAD_INTENT_TERMS.iter().any(|t| q.contains(t))
}

// ── Project-overview composer ─────────────────────────────────────────────────

/// Safely truncate `s` to at most `max_chars` chars at a UTF-8 boundary.
pub(crate) fn truncate_on_boundary(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((i, _)) => &s[..i],
    }
}

/// Compute the nearest common ancestor (deepest shared directory prefix) of a
/// set of file paths (using `/` as the separator). Returns `None` when `paths`
/// is empty or has no common prefix.
pub(crate) fn common_ancestor(paths: &[String]) -> Option<String> {
    let mut iter = paths.iter();
    let first = iter.next()?;
    let mut prefix: Vec<&str> = first.trim_end_matches('/').split('/').collect();
    for path in iter {
        let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        let shared = prefix
            .iter()
            .zip(segs.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            return None;
        }
    }
    Some(prefix.join("/"))
}

/// Build a "PROJECT OVERVIEW" block from directory roll-up summaries. Runs
/// **inside the sync store scope** so it never crosses an `.await`. Returns an
/// empty string when no dir summaries exist (the feature is then completely inert).
///
/// Budget split:
/// - `overview_budget` chars max for the overview block.
/// - Callers subtract `result.len()` from their chunk budget.
pub fn build_project_overview(
    store: &Store,
    hits: &[SearchHit],
    scope: Option<&str>,
    overview_budget: usize,
) -> String {
    if overview_budget == 0 {
        return String::new();
    }
    // When no scope is provided we need hits to derive the root; when a scope is provided
    // explicitly we can build an overview without any hits (e.g. standalone MCP tool).
    if hits.is_empty() && scope.is_none() {
        return String::new();
    }

    // Determine the root directory to summarise: prefer the explicit scope, else
    // the nearest common ancestor of the top hit paths.
    let root: String = if let Some(s) = scope {
        s.to_owned()
    } else {
        let top_paths: Vec<String> = hits.iter().take(5).map(|h| h.entry_path.clone()).collect();
        match common_ancestor(&top_paths) {
            Some(a) => a,
            None => return String::new(),
        }
    };

    // Look up the root directory's summary. If missing, walk up to find one.
    let root_rec = {
        let mut r = None;
        let mut candidate = root.clone();
        loop {
            if let Ok(Some(rec)) = store.summary_by_path(&candidate) {
                if rec.kind == "dir" {
                    r = Some(rec);
                    break;
                }
            }
            // Walk up one level.
            match std::path::Path::new(&candidate).parent() {
                Some(p) => {
                    let s = p.to_string_lossy().into_owned();
                    if s.is_empty() || s == "/" || s == candidate {
                        break;
                    }
                    candidate = s;
                }
                None => break,
            }
        }
        r
    };

    let root_rec = match root_rec {
        Some(r) => r,
        None => return String::new(),
    };

    // Compose the overview block.
    let root_name = std::path::Path::new(&root_rec.path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&root_rec.path);
    let root_summary = if root_rec.summary.trim().is_empty() {
        return String::new();
    } else {
        root_rec.summary.trim()
    };

    // Detected application/structure per directory (v0.66), so a broad answer can say
    // "this folder is a Django app". One indexed query, primaries only; fail-open.
    let app_by_dir: std::collections::HashMap<String, String> = store
        .primary_apps_under(&root_rec.path)
        .map(|apps| apps.into_iter().map(|a| (a.path, a.app_name)).collect())
        .unwrap_or_default();
    let app_tag = |path: &str| -> String {
        app_by_dir
            .get(path)
            .map(|n| format!(" [{n}]"))
            .unwrap_or_default()
    };

    let mut block = format!(
        "PROJECT OVERVIEW (directory roll-up summaries — background context; \
         cite numbered excerpts below for specific claims):\n{root_name}{}: {root_summary}\n",
        app_tag(&root_rec.path)
    );

    // Append top child-directory L0 abstracts (one-liners) if budget allows.
    if let Ok(children) = store.children_summaries(&root_rec.path) {
        for child in children.iter().filter(|c| c.kind == "dir").take(6) {
            let child_name = std::path::Path::new(&child.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&child.path);
            let child_l0 = child.summary_l0.as_deref().unwrap_or(&child.summary).trim();
            if child_l0.is_empty() {
                continue;
            }
            let line = format!("  - {child_name}{}: {child_l0}\n", app_tag(&child.path));
            if block.len() + line.len() > overview_budget {
                break;
            }
            block.push_str(&line);
        }
    }

    // Hard-cap to overview_budget chars at a UTF-8 boundary.
    truncate_on_boundary(&block, overview_budget).to_owned()
}

/// Phrases that signal a question is about implementation/code, not prose. (v0.39)
const CODE_INTENT_TERMS: [&str; 12] = [
    "function",
    "implement",
    "method",
    "struct",
    "trait",
    "fn ",
    "class ",
    "def ",
    "which file",
    "in the code",
    "code that",
    "where is",
];

/// How hard to lift a code-file hit when the question is code-intent. Modest and
/// multiplicative — enough to put the implementing file above prose docs of similar
/// raw score, not enough to drag in an unrelated code file.
const CODE_INTENT_BOOST: f64 = 1.6;

/// Code file extensions: a hit here is "the implementation" for a code question.
const CODE_EXTS: [&str; 22] = [
    "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "go", "java", "c", "h", "cpp", "cc", "rb",
    "php", "swift", "kt", "scala", "cs", "sh", "lua",
];

/// Does the question ask about implementation/code? True on explicit code phrasing or a
/// snake_case identifier (≥4 chars with an underscore) — a strong "they mean a symbol" tell.
pub(crate) fn is_code_intent(question: &str) -> bool {
    let q = question.to_ascii_lowercase();
    CODE_INTENT_TERMS.iter().any(|t| q.contains(t))
        || question
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w.len() >= 4 && w.contains('_'))
}

fn is_code_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| CODE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Multiplicatively lift code-file hits for code-intent questions, then the caller's
/// re-sort orders them. No-op when the question isn't code-intent or no hit is code.
pub(crate) fn apply_code_intent_boost(hits: &mut [SearchHit], question: &str) {
    if !is_code_intent(question) {
        return;
    }
    for h in hits.iter_mut() {
        if is_code_path(&h.entry_path) {
            h.rrf_score *= CODE_INTENT_BOOST;
        }
    }
}

/// True if any `/`-segment of `path` is a historical marker (see `HISTORICAL_SEGMENTS`).
pub(crate) fn path_is_historical(path: &str) -> bool {
    path.split('/')
        .any(|seg| HISTORICAL_SEGMENTS.contains(&seg.to_ascii_lowercase().as_str()))
}

/// Multiply down the score of hits under a historical path — unless the query is explicitly
/// scoped *into* such a path, in which case the user is asking for the history, so leave it.
pub(crate) fn apply_archive_penalty(hits: &mut [SearchHit], scope: Option<&str>) {
    if scope.map(path_is_historical).unwrap_or(false) {
        return;
    }
    for h in hits.iter_mut() {
        if path_is_historical(&h.entry_path) {
            h.rrf_score *= ARCHIVE_PENALTY;
        }
    }
}
