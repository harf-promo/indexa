//! Detectors: turn the uncertainty signals the pipeline already produces
//! (duplicate clusters, mid-band Tier-0 confidence) into open ledger questions.
//!
//! The classification detectors fire inline in `cmd_classify` (they need the
//! Tier-0 hint maps that only exist there) and the summary-drift detector fires
//! inline in `summarize_file` (it needs both embeddings in hand);
//! [`run_detectors`] is the standalone pass appended to `cmd_index` and covers
//! the duplicate, archive, language-fallback, and symbol-ambiguity detectors
//! plus the crash-repair and expiry sweeps.

use crate::config::ReviewConfig;
use crate::store::{abstract_from, DuplicateCluster, NewDecision, Store, SummaryRecord};
use anyhow::Result;
use sha2::{Digest, Sha256};

use super::DecisionType;

/// Lower bound of the ask-band: below this, Tier-0 itself refuses to classify
/// (`TIER0_AGGREGATION_THRESHOLD`), so a question would have no suggestion to
/// confirm. The band is `[UNCERTAINTY_FLOOR, review.auto_record_below)`.
pub const UNCERTAINTY_FLOOR: f32 = 0.6;

/// Near-duplicate similarity threshold for the duplicate detector. Stricter than
/// the insights default (0.85): a *question* interrupts the user, so it should
/// only fire on clusters that are almost certainly copies.
const NEAR_DUP_THRESHOLD: f32 = 0.95;

/// Staleness horizon for the archive detector — matches the insights default
/// (`find_stale_entries(365)`), so the detector asks about exactly what the
/// insights tab shows as stale.
const ARCHIVE_STALE_DAYS: i64 = 365;

/// Evidence-bucket width for archive questions. A `keep_active` answer is keyed
/// to the bucket, so the question naturally returns when the dir ages into the
/// next one (~every 3 months of continued inactivity) — no timer code needed.
const ARCHIVE_BUCKET_DAYS: i64 = 90;

/// Cosine below which a regenerated same-content summary counts as drifted.
/// 0.80 is deliberately low: same-model re-runs of identical content typically
/// land > 0.9, so only a real disagreement (model switch, prompt change, LLM
/// mood swing) interrupts the user.
const DRIFT_COSINE_THRESHOLD: f32 = 0.80;

/// A language question only fires for files with at least this many untagged
/// chunks — a 1-chunk file isn't worth an interruption.
const LANGUAGE_MIN_CHUNKS: i64 = 3;

/// Bounded sweep size for the language detector's candidate query (the
/// per-scan caps below bound how many *questions* open; this bounds the scan).
const LANGUAGE_SCAN_LIMIT: usize = 200;

/// Top-K ambiguous symbols considered per scan, ranked by caller count — the
/// hottest ambiguities first; the rest wait for a later scan.
const SYMBOL_AMBIGUITY_TOP_K: usize = 10;

/// Above this many definers a symbol is an idiom (every type defines its own
/// `new`/`default`), not a resolvable ambiguity — asking is pure noise. (v0.39)
const SYMBOL_AMBIGUITY_MAX_DEFINERS: usize = 6;

/// Extensions whose "duplicates" are not actionable: a user never picks a
/// canonical copy among near-identical images/fonts/binaries — they're assets,
/// not redundant source. (v0.39 duplicate-noise filter.)
const DUP_SKIP_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif", "ico", "icns", "svg", "heic", "pdf",
    "mp4", "mov", "avi", "mkv", "webm", "mp3", "wav", "flac", "woff", "woff2", "ttf", "otf", "eot",
    "zip", "gz", "tar", "bin", "wasm", "class", "o", "a", "dylib", "so", "lock",
];

/// Path fragments marking generated / vendored / asset trees: members here are
/// regenerated on build (icon sets) or are intentional collections — "dedupe
/// these" is never the right ask. (v0.39 duplicate-noise filter.)
const DUP_SKIP_DIR_FRAGMENTS: &[&str] = &[
    ".xcassets/",
    "/icons/",
    "/assets/",
    "/dist/",
    "/build/",
    "/node_modules/",
    "/vendor/",
    "/.next/",
    "/target/",
    "/competitors/",
];

/// Universal trait/idiom method names: legitimately defined independently by many
/// types, so "which is authoritative?" has no answer. (v0.39 symbol-noise filter.)
const IDIOM_SYMBOLS: &[&str] = &[
    "new",
    "default",
    "parse",
    "build",
    "from",
    "into",
    "from_str",
    "as_str",
    "as_ref",
    "as_mut",
    "clone",
    "to_string",
    "to_owned",
    "drop",
    "deref",
    "deref_mut",
    "fmt",
    "eq",
    "ne",
    "hash",
    "cmp",
    "partial_cmp",
    "next",
    "len",
    "is_empty",
    "iter",
    "into_iter",
    "default_config_path",
    "main",
    "run",
    "init",
    "setup",
    "render",
    "update",
    "handle",
    "call",
    "apply",
    "load",
    "save",
    "open",
    "close",
    "read",
    "write",
    "flush",
    "poll",
    "start",
    "stop",
    "name",
    "kind",
    "value",
];

/// A symbol so ubiquitous that disambiguating it is busywork: a known idiom, or a
/// common accessor/builder prefix (`with_`/`set_`/`get_`/`is_`/`to_`/`from_`/`on_`).
fn is_idiom_symbol(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    IDIOM_SYMBOLS.contains(&n.as_str())
        || [
            "with_", "set_", "get_", "is_", "to_", "from_", "on_", "try_",
        ]
        .iter()
        .any(|p| n.starts_with(p))
}

fn dup_ext_is_asset(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| DUP_SKIP_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn dup_in_generated_dir(path: &str) -> bool {
    DUP_SKIP_DIR_FRAGMENTS.iter().any(|f| path.contains(f))
}

/// Is a duplicate cluster worth a human's attention? Not when every member is an
/// asset/binary (you won't "pick a canonical" screenshot) or any member lives in a
/// generated/vendored tree (icon sets regenerate; vendored copies aren't yours).
/// Only redundant source/text that a user could actually consolidate qualifies.
fn duplicate_cluster_actionable(paths: &[String]) -> bool {
    let all_assets = !paths.is_empty() && paths.iter().all(|p| dup_ext_is_asset(p));
    let any_generated = paths.iter().any(|p| dup_in_generated_dir(p));
    !(all_assets || any_generated)
}

/// Extract the file name (basename without directory) from a path string.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Do all members of a near-duplicate cluster share the same filename? A
/// near-dup (similarity-based, not content-identical) cluster of differently-
/// named files is almost certainly a false positive — two files with similar
/// *topic* whose summaries land nearby in embedding space, not actual copies.
/// We only ask when every member has the same basename (e.g. `qa.rs` appearing
/// in two different crates), which is strong evidence of an unintentional copy.
/// Exact clusters (identical content fingerprint) skip this check — two files
/// with different names but byte-identical content really are duplicates. (v0.40)
fn near_dup_same_basenames(paths: &[String]) -> bool {
    let mut it = paths.iter().map(|p| basename(p));
    match it.next() {
        None => true,
        Some(first) => it.all(|b| b == first),
    }
}

/// Symbol-ambiguity candidates, gated by config and pre-filtered for idioms.
/// Returns empty when the feature is off (the default), so the detector loop is a
/// no-op and never opens an unanswerable "which `new` is authoritative?" question. (v0.39)
fn symbol_ambiguity_candidates(store: &Store, cfg: &ReviewConfig) -> Result<Vec<(String, i64)>> {
    if !cfg.symbol_ambiguity {
        return Ok(Vec::new());
    }
    Ok(store
        .ambiguous_called_symbols(SYMBOL_AMBIGUITY_TOP_K)?
        .into_iter()
        .filter(|(sym, _)| !is_idiom_symbol(sym))
        .collect())
}

/// Retroactively dismiss already-open questions the v0.39 noise filters would now
/// reject — so existing inboxes get quiet without a re-index. Run from both
/// `run_detectors` (so a re-index cleans up) and `indexa prune` (cheap, no Ollama).
/// Dismisses: every `symbol_ambiguity` row when the feature is off, plus idiom/over-
/// definer ones when on; and `duplicate` rows whose cluster isn't actionable. With
/// `dry_run`, counts without dismissing (for `prune --dry-run`). (v0.39)
pub fn sweep_filtered_noise(store: &mut Store, cfg: &ReviewConfig, dry_run: bool) -> Result<usize> {
    let mut hits = 0usize;
    for d in store.open_decisions(None, 10_000)? {
        let drop = if d.decision_type == DecisionType::SymbolAmbiguity.as_str() {
            !cfg.symbol_ambiguity || is_idiom_symbol(&d.subject)
        } else if d.decision_type == DecisionType::Duplicate.as_str() {
            let params: serde_json::Value = serde_json::from_str(&d.params).unwrap_or_default();
            let paths: Vec<String> = params
                .get("paths")
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            // Asset/generated filter from v0.39.
            let noisy_asset = !paths.is_empty() && !duplicate_cluster_actionable(&paths);
            // Near-dup (similarity < 1.0) clusters whose members have different
            // basenames are false positives (similar topics, not copies). (v0.40)
            let similarity = params
                .get("similarity")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0);
            let near_dup_false_pos =
                similarity < 1.0 && !paths.is_empty() && !near_dup_same_basenames(&paths);
            noisy_asset || near_dup_false_pos
        } else {
            false
        };
        if drop {
            hits += 1;
            if !dry_run {
                store.dismiss_decision(d.id)?;
            }
        }
    }
    Ok(hits)
}

/// What a detector pass did. Totals are across detector types; a per-type
/// split waits until a surface actually needs it.
#[derive(Debug, Default, Clone, Copy)]
pub struct DetectorReport {
    /// Questions opened this pass.
    pub opened: usize,
    /// Candidates skipped: already covered by a live decision, deduped against
    /// an existing open question, or sticky-dismissed with unchanged evidence.
    pub skipped: usize,
    /// Decided rows whose projection was re-run by the crash-repair sweep.
    pub repaired: usize,
    /// Open questions expired because their evidence left the index.
    pub expired: usize,
}

/// The detector pass run at the end of `cmd_index`: repair sweep first (so a
/// crashed projection heals before new questions stack on top), then the
/// duplicate, archive, language, and symbol detectors — in that order, so the
/// higher-priority question types get the cap budget first — honoring the
/// fatigue caps in `cfg`.
pub fn run_detectors(store: &mut Store, cfg: &ReviewConfig) -> Result<DetectorReport> {
    let mut report = DetectorReport {
        repaired: super::repair_unapplied(store)?,
        ..DetectorReport::default()
    };

    // v0.39: retroactively dismiss already-open questions the noise filters now reject
    // (idiom / disabled symbol_ambiguity, asset/generated duplicate clusters) so an
    // existing inbox gets quiet on the next index without a manual sweep.
    report.skipped += sweep_filtered_noise(store, cfg, false)?;

    // Expiry sweep: an open question whose evidence left the index would
    // otherwise linger forever and permanently consume the open budget —
    // starving new questions by attrition. "Left the index" = a member path
    // has neither an entries row NOR a summary row (the deep-without-scan
    // workflow legitimately produces summaries with no entries row, so
    // entries-absence alone is not evidence of removal). Expired is recorded,
    // never silently dropped; and expiry is not a sticky dismissal, so the
    // question returns if the evidence does.
    for d in store.open_decisions(None, cfg.max_open.max(64))? {
        let params: serde_json::Value = serde_json::from_str(&d.params).unwrap_or_default();
        let members: Vec<String> = params
            .get("paths")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_else(|| vec![d.subject.clone()]);
        let mut vanished = None;
        for m in &members {
            if !store.entry_exists(m)? && !store.summary_exists(m)? {
                vanished = Some(m.clone());
                break;
            }
        }
        if let Some(gone) = vanished {
            store.expire_decision(d.id, &format!("{gone} left the index"))?;
            report.expired += 1;
        }
    }

    // Exact clusters first: they are certain, so they deserve the cap budget
    // before the probabilistic near-duplicates.
    let mut clusters = store.find_exact_duplicates()?;
    clusters.extend(store.find_near_duplicates(NEAR_DUP_THRESHOLD)?);

    let mut open_budget = (cfg.max_open as i64 - store.open_decision_count()?).max(0) as usize;
    for cluster in clusters {
        if cluster.paths.len() < 2 {
            continue;
        }
        // Skip non-actionable clusters: near-identical assets (icon sets, competitor
        // screenshots, fonts) and generated/vendored copies are not redundant source a
        // user would consolidate — asking floods the inbox with unanswerable questions. (v0.39)
        if !duplicate_cluster_actionable(&cluster.paths) {
            report.skipped += 1;
            continue;
        }
        // Near-dup clusters of differently-named files are almost always false
        // positives: two files on the same topic whose summaries land nearby in
        // embedding space. Only ask when all members share a basename (e.g.
        // `qa.rs` in two crates) or the cluster is exact-content. (v0.40)
        if !cluster.exact && !near_dup_same_basenames(&cluster.paths) {
            report.skipped += 1;
            continue;
        }
        if report.opened >= cfg.max_new_per_scan || open_budget == 0 {
            break;
        }
        // A live decision (open, or decided and un-superseded) touching ANY
        // member already covers this cluster — re-asking would nag.
        let mut covered = false;
        for p in &cluster.paths {
            if !store.decisions_touching_path(p)?.is_empty() {
                covered = true;
                break;
            }
        }
        if covered {
            report.skipped += 1;
            continue;
        }
        match store.record_decision(duplicate_question(&cluster))? {
            Some(_) => {
                report.opened += 1;
                open_budget -= 1;
            }
            None => report.skipped += 1,
        }
    }

    // Archive detector: dirs untouched past the staleness horizon, same caps.
    // Candidates come from the same query the insights tab shows
    // (`find_stale_entries`), filtered to:
    // - dirs with a KNOWN mtime (NULL means unknown, not evidence of age);
    // - top-level-ish dirs only — a candidate is dropped when another stale
    //   candidate is its ancestor (ask about /old once; the answer's dir
    //   weight covers the subtree, so per-subdir questions would only nag).
    let mut stale: Vec<(String, i64)> = store
        .find_stale_entries(ARCHIVE_STALE_DAYS)?
        .into_iter()
        .filter(|e| e.kind == "dir" && e.modified_s.is_some())
        .map(|e| (e.path, e.days_since_modified))
        .collect();
    // Lexicographic order puts an ancestor before every path under it, so one
    // pass against the kept list implements the ancestor filter.
    stale.sort_unstable();
    let mut kept: Vec<(String, i64)> = Vec::new();
    for (path, days) in stale {
        if !kept.iter().any(|(k, _)| is_path_ancestor(k, &path)) {
            kept.push((path, days));
        }
    }
    for (path, days) in kept {
        if report.opened >= cfg.max_new_per_scan || open_budget == 0 {
            break;
        }
        // Already where the user wants it: an archive/system dir needs no
        // question (`archive` is also what answering "archive" projects, so a
        // decided question self-suppresses here on the next pass).
        if let Some(c) = store.classification_for(&path)? {
            if c.category == "archive" || c.category == "system" {
                report.skipped += 1;
                continue;
            }
        }
        let files = store.count_files_under(&path)?;
        let fingerprint = archive_fingerprint(days, files);
        // Covered check, with one carve-out: a decided `keep_active` archive
        // row whose evidence bucket has since moved becomes a chained re-ask
        // (never a parentless second head for the key) — that is the promised
        // "keep_active gets re-asked when the dir ages into the next bucket".
        let mut covered = false;
        let mut reask_parent: Option<i64> = None;
        for id in store.decisions_touching_path(&path)? {
            let Some(d) = store.decision_by_id(id)? else {
                continue;
            };
            if d.decision_type == DecisionType::Archive.as_str()
                && d.status == "decided"
                && d.chosen.as_deref() == Some("keep_active")
            {
                if d.evidence_hash == fingerprint {
                    covered = true;
                } else {
                    reask_parent = Some(d.id);
                }
            } else {
                covered = true;
            }
        }
        if covered {
            report.skipped += 1;
            continue;
        }
        let q = archive_question(&path, days, files);
        let opened = match reask_parent {
            Some(prior) => store.supersede_with(prior, q)?,
            None => store.record_decision(q)?,
        };
        match opened {
            Some(_) => {
                report.opened += 1;
                open_budget -= 1;
            }
            None => report.skipped += 1,
        }
    }

    // Language-fallback detector (priority 20). Implemented as a cheap
    // post-hoc heuristic over the chunks table — chunks whose `language` IS
    // NULL on a file whose extension says "code" — rather than plumbing a
    // per-file fallback flag from the parsers up through the deep path: the
    // parse results cross three crates (parsers → cli/web deep loops → store)
    // and every Extracted consumer's signature would have churned for a flag
    // the detector can derive from what's already persisted. The two are
    // equivalent because the tree-sitter CodeParser always tags its chunks;
    // an untagged chunk on a code extension *is* the fallback-to-text case.
    for (path, n_chunks) in store.unlabeled_chunk_files(LANGUAGE_MIN_CHUNKS, LANGUAGE_SCAN_LIMIT)? {
        if report.opened >= cfg.max_new_per_scan || open_budget == 0 {
            break;
        }
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let Some(lang) = ext.as_deref().and_then(code_language_for_extension) else {
            continue; // not code-shaped — plain text is correct untagged
        };
        if let Some(prior) = store.latest_decided(DecisionType::Language.as_str(), &path)? {
            // A re-deep rewrote the chunks (language NULL again) — re-apply the
            // standing answer silently instead of asking twice; "ignore"
            // projects to nothing, which also (correctly) re-suppresses.
            let fx = super::effects::apply_decision_effects(store, &prior)?;
            store.mark_effects_applied(prior.id, &fx)?;
            report.skipped += 1;
            continue;
        }
        match store.record_decision(language_question(&path, lang, n_chunks))? {
            Some(_) => {
                report.opened += 1;
                open_budget -= 1;
            }
            None => report.skipped += 1,
        }
    }

    // Symbol-ambiguity detector (priority 20): bare names with `calls` edges
    // that are *defined* in more than one file — exactly the case where the
    // bare-name call graph (who_calls / blast_radius) can't tell definitions
    // apart. Top-K hottest by caller count per scan; the answer is stored as
    // the question's effects only (no projection table — graph surfaces
    // consult the ledger's answer separately).
    for (symbol, callers) in symbol_ambiguity_candidates(store, cfg)? {
        if report.opened >= cfg.max_new_per_scan || open_budget == 0 {
            break;
        }
        let definers = store.edges_to("defines", &symbol)?;
        // < 2: racing re-deep shrank the set. > MAX: an idiom every type defines
        // (`new`, `default`) — not a resolvable ambiguity, so don't ask. (v0.39)
        if definers.len() < 2 || definers.len() > SYMBOL_AMBIGUITY_MAX_DEFINERS {
            continue;
        }
        let fingerprint = symbol_fingerprint(&definers);
        let mut reask_parent: Option<i64> = None;
        if let Some(prior) =
            store.latest_decided(DecisionType::SymbolAmbiguity.as_str(), &symbol)?
        {
            if prior.evidence_hash == fingerprint {
                // The definer set hasn't moved — the standing answer covers it.
                report.skipped += 1;
                continue;
            }
            // Definer set changed → chained re-ask, never a second head.
            reask_parent = Some(prior.id);
        }
        let q = symbol_ambiguity_question(&symbol, &definers, callers);
        let opened = match reask_parent {
            Some(prior) => store.supersede_with(prior, q)?,
            None => store.record_decision(q)?,
        };
        match opened {
            Some(_) => {
                report.opened += 1;
                open_budget -= 1;
            }
            None => report.skipped += 1,
        }
    }
    Ok(report)
}

/// Is `ancestor` a directory ancestor of `path`? Boundary-aware: `/proj` is not
/// an ancestor of `/projector` (the `/proj` vs `/projector` LIKE-prefix trap).
fn is_path_ancestor(ancestor: &str, path: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
}

/// Build the open question for a duplicate cluster. Subject = first sorted
/// member path (stable across runs even when similarity wiggles); options =
/// every member ("this one is canonical") plus `keep_all`.
fn duplicate_question(cluster: &DuplicateCluster) -> NewDecision {
    let mut paths = cluster.paths.clone();
    paths.sort_unstable();
    let mut options: Vec<String> = paths.clone();
    options.push("keep_all".to_owned());
    NewDecision {
        decision_type: DecisionType::Duplicate.as_str().to_owned(),
        subject: paths[0].clone(),
        params: serde_json::json!({
            "paths": paths,
            "similarity": cluster.similarity,
            "exact": cluster.exact,
        }),
        options: serde_json::json!(options),
        auto_value: Some(paths[0].clone()),
        confidence: Some(cluster.similarity),
        evidence_hash: duplicate_fingerprint(&paths, cluster.exact, cluster.similarity),
        priority: 60,
        paths,
    }
}

/// Build the open question for one stale dir. Evidence is deliberately coarse:
/// the staleness bucket plus the subtree file count — neither moves on a
/// re-scan of an untouched dir, so a dismissed/answered question stays quiet
/// until the dir genuinely ages (next bucket) or its contents change.
fn archive_question(path: &str, days: i64, files: i64) -> NewDecision {
    NewDecision {
        decision_type: DecisionType::Archive.as_str().to_owned(),
        subject: path.to_owned(),
        params: serde_json::json!({ "days": days, "files": files }),
        options: serde_json::json!(["archive", "keep_active"]),
        auto_value: Some("archive".to_owned()),
        confidence: None,
        evidence_hash: archive_fingerprint(days, files),
        // Below classification (50) and duplicates (60): an idle dir is the
        // least urgent thing in the inbox.
        priority: 30,
        paths: vec![path.to_owned()],
    }
}

// ── Summary drift (fired inline from summarize_file, not run_detectors) ──────

/// Open a summary-drift question when a regeneration of IDENTICAL content
/// produced a summary that semantically disagrees with the old one
/// (cosine < [`DRIFT_COSINE_THRESHOLD`]). Called from `summarize_file` AFTER
/// the new row is written — the question never blocks the write; it asks
/// "keep new / restore old" after the fact.
///
/// Skips silently (Ok(None)) when either embedding is missing (nothing to
/// compare — and a `restore_old` projection clears the row's embedding, so a
/// later regen of the restored row also lands here instead of nag-looping).
/// Dedup: an open row for the path blocks via the partial unique index; a
/// decided row with the same evidence (same content hash + model) means the
/// user already chose for exactly this regeneration; a decided row with
/// different evidence becomes a chained re-ask — never a second head.
pub fn flag_summary_drift(
    store: &mut Store,
    old: &SummaryRecord,
    new: &SummaryRecord,
) -> Result<Option<i64>> {
    let (Some(old_emb), Some(new_emb)) = (old.embedding.as_deref(), new.embedding.as_deref())
    else {
        return Ok(None);
    };
    let c = cosine(old_emb, new_emb);
    if c >= DRIFT_COSINE_THRESHOLD {
        return Ok(None);
    }
    let fingerprint = drift_fingerprint(&new.source_hash, &new.model);
    let mut reask_parent: Option<i64> = None;
    if let Some(prior) = store.latest_decided(DecisionType::SummaryDrift.as_str(), &new.path)? {
        if prior.evidence_hash == fingerprint {
            return Ok(None);
        }
        reask_parent = Some(prior.id);
    }
    let old_l0 = old
        .summary_l0
        .clone()
        .unwrap_or_else(|| abstract_from(&old.summary));
    let q = NewDecision {
        decision_type: DecisionType::SummaryDrift.as_str().to_owned(),
        subject: new.path.clone(),
        // The full old summary is stashed in params because `restore_old`'s
        // projection re-writes it from here — the summaries row already holds
        // the NEW text by the time anyone answers.
        params: serde_json::json!({
            "old_summary": old.summary,
            "old_l0": old_l0,
            "new_l0": abstract_from(&new.summary),
            "cosine": c,
            "old_model": old.model,
            "new_model": new.model,
        }),
        options: serde_json::json!(["keep_new", "restore_old"]),
        // The new summary already landed — keeping it is the no-action default.
        auto_value: Some("keep_new".to_owned()),
        confidence: None,
        evidence_hash: fingerprint,
        // Above archive (30), below classification (50): drift is a quality
        // regression on data the user already trusted, but nothing is lost
        // while the question waits.
        priority: 40,
        paths: vec![new.path.clone()],
    };
    match reask_parent {
        Some(prior) => store.supersede_with(prior, q),
        None => store.record_decision(q),
    }
}

/// Drift evidence fingerprint: the content hash + the model that produced the
/// new summary. A decided answer stays standing for this exact (bytes, model)
/// pair; switching models or changing the file re-arms the question.
pub fn drift_fingerprint(source_hash: &str, model: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_hash.as_bytes());
    hasher.update([0u8]);
    hasher.update(model.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ── Language fallback ─────────────────────────────────────────────────────────

/// Build the open question for one untagged code file. `lang` is the
/// extension-derived guess; chunk count is the evidence weight shown to the
/// user. A hyperpolyglot content-detection candidate is appended when it
/// disagrees with the extension (best-effort: the file must still exist).
fn language_question(path: &str, lang: &'static str, n_chunks: i64) -> NewDecision {
    let mut options: Vec<String> = vec![lang.to_owned()];
    // Content-based second opinion. hyperpolyglot reads the file; an unreadable
    // or vanished file simply contributes no candidate.
    if let Ok(Some(detection)) = hyperpolyglot::detect(std::path::Path::new(path)) {
        let candidate = detection.language().to_ascii_lowercase();
        if candidate != lang {
            options.push(candidate);
        }
    }
    options.push("ignore".to_owned());
    NewDecision {
        decision_type: DecisionType::Language.as_str().to_owned(),
        subject: path.to_owned(),
        params: serde_json::json!({ "language": lang, "chunks": n_chunks }),
        options: serde_json::json!(options),
        auto_value: Some(lang.to_owned()),
        confidence: None,
        // Keyed to the guess + chunk count: a re-chunk that changes the file's
        // shape re-arms a dismissed question; an untouched file stays quiet.
        evidence_hash: language_fingerprint(lang, n_chunks),
        // Lowest priority alongside symbol ambiguity: a missing tag degrades
        // stats/filters, not retrieval.
        priority: 20,
        paths: vec![path.to_owned()],
    }
}

/// Language evidence fingerprint: the extension-derived guess + chunk count.
fn language_fingerprint(lang: &str, n_chunks: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(lang.as_bytes());
    hasher.update([0u8]);
    hasher.update(n_chunks.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extension → language name for files that *should* be code. Mirrors the
/// CodeParser's names for its 7 tree-sitter languages and extends to common
/// languages the parser has no grammar for (which is exactly when chunks fall
/// back to untagged plain text). Deliberately a short curated list — the
/// detector asks questions, so a wrong mapping nags; obscure extensions can
/// wait until someone hits them.
fn code_language_for_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        // Tree-sitter-covered (normally tagged; appear here only if an older
        // index or a non-code parser produced the chunks).
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "java" => "java",
        // No grammar shipped → the actual fallback cases.
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "shell",
        "pl" | "pm" => "perl",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "clj" | "cljs" => "clojure",
        "vue" => "vue",
        "svelte" => "svelte",
        "sql" => "sql",
        "zig" => "zig",
        "jl" => "julia",
        "nim" => "nim",
        "ml" | "mli" => "ocaml",
        "fs" | "fsx" => "fsharp",
        "groovy" => "groovy",
        "m" => "objective-c",
        _ => return None,
    })
}

// ── Symbol ambiguity ──────────────────────────────────────────────────────────

/// Build the open question for one ambiguous symbol. Subject = the bare symbol
/// name; options = every defining file ("this one is authoritative") plus
/// `all`. `params.paths` carries the definers so the expiry sweep checks THEM
/// against the index (the subject is a symbol, not a path — without this the
/// sweep would expire the question immediately).
fn symbol_ambiguity_question(symbol: &str, definers: &[String], callers: i64) -> NewDecision {
    let mut sorted = definers.to_vec();
    sorted.sort_unstable();
    let mut options: Vec<String> = sorted.clone();
    options.push("all".to_owned());
    NewDecision {
        decision_type: DecisionType::SymbolAmbiguity.as_str().to_owned(),
        subject: symbol.to_owned(),
        params: serde_json::json!({
            "definers": sorted,
            "callers": callers,
            "paths": sorted,
        }),
        options: serde_json::json!(options),
        auto_value: None, // no defensible automatic pick between definitions
        confidence: None,
        evidence_hash: symbol_fingerprint(&sorted),
        priority: 20,
        paths: sorted,
    }
}

/// Symbol evidence fingerprint: the sorted definer set. The question (or a
/// standing answer) stays quiet until a definition is added or removed.
fn symbol_fingerprint(definers: &[String]) -> String {
    let mut sorted: Vec<&str> = definers.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    for p in sorted {
        hasher.update(p.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

/// Archive evidence fingerprint: staleness bucketed to [`ARCHIVE_BUCKET_DAYS`]
/// plus the subtree file count. Pub so the insights "don't ask about this"
/// endpoint can record the byte-identical hash the detector would.
pub fn archive_fingerprint(days: i64, files: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update((days / ARCHIVE_BUCKET_DAYS).to_le_bytes());
    hasher.update(files.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// Duplicate-cluster evidence fingerprint: sorted member paths, the exact flag,
/// and similarity rounded to 0.01. A dismissed cluster question only returns
/// when membership changes or similarity moves by a visible amount. Pub for the
/// same reason as [`archive_fingerprint`] (insights pre-dismissal).
pub fn duplicate_fingerprint(sorted_paths: &[String], exact: bool, similarity: f32) -> String {
    let mut hasher = Sha256::new();
    for p in sorted_paths {
        hasher.update(p.as_bytes());
        hasher.update([0u8]);
    }
    hasher.update(if exact { "exact" } else { "near" });
    hasher.update(((similarity * 100.0).round() as i64).to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// Classification evidence fingerprint: the dir's own surface hint + its
/// child-hint histogram as shares rounded to 0.05. Coarse on purpose — adding
/// one file to a 40-file folder must NOT change the fingerprint (no re-ask),
/// while a real composition shift must. Shares that round to zero (< 2.5%) are
/// omitted entirely, so a single stray file can't introduce a new histogram key.
pub fn classification_fingerprint(own_hint: Option<&str>, children: &[(String, i64)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(own_hint.unwrap_or("").as_bytes());
    hasher.update([0u8]);

    let total: i64 = children.iter().map(|(_, n)| *n).sum();
    if total > 0 {
        let mut buckets: Vec<(&str, i64)> = children
            .iter()
            .filter_map(|(cat, n)| {
                let bucket = ((*n as f64 / total as f64) / 0.05).round() as i64;
                (bucket > 0).then_some((cat.as_str(), bucket))
            })
            .collect();
        // Caller order must not matter (histogram rows come from a HashMap).
        buckets.sort_unstable();
        for (cat, bucket) in buckets {
            hasher.update(cat.as_bytes());
            hasher.update(bucket.to_le_bytes());
            hasher.update([0u8]);
        }
    }
    format!("{:x}", hasher.finalize())
}

// ── Pre-dismissal (insights → ledger) ─────────────────────────────────────────
// The insights tab's "don't ask about this" action: record the question the
// detector WOULD raise, already in the dismissed state, so sticky dismissal
// suppresses it before it is ever asked.

/// Pre-dismiss a duplicate-cluster question. Routed through
/// [`duplicate_question`] so the evidence hash is computed by the exact code
/// path the detector uses. Exact clusters (`exact=true`, similarity 1.0) are
/// suppressed deterministically; near-dup suppression is best-effort — the
/// detector recomputes membership and average similarity at its own threshold,
/// and on disagreement the question appears once and can be dismissed from the
/// inbox (fail-open, never fail-silent).
pub fn predismiss_duplicate(store: &mut Store, paths: &[String]) -> Result<bool> {
    // Server-authoritative: re-derive the clusters from the detector's OWN
    // sources and dismiss those. Trusting caller-supplied exact/similarity can
    // never work — sticky dismissal requires a byte-identical evidence_hash,
    // and the insights UI clusters at a different threshold than the detector
    // (and never knows the exact/near tag). Any detector cluster sharing two
    // or more of the given paths is dismissed; membership drift later changes
    // the fingerprint and the question may return — by design.
    let given: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut clusters = store.find_exact_duplicates()?;
    clusters.extend(store.find_near_duplicates(NEAR_DUP_THRESHOLD)?);
    let mut dismissed_any = false;
    for cluster in clusters {
        let overlap = cluster
            .paths
            .iter()
            .filter(|p| given.contains(p.as_str()))
            .count();
        if overlap >= 2 {
            record_predismissed(store, duplicate_question(&cluster))?;
            dismissed_any = true;
        }
    }
    Ok(dismissed_any)
}

/// Pre-dismiss the archive question for one stale dir. Returns `false` (no row
/// recorded) when the dir has no entries row or no known mtime — exactly the
/// cases the detector itself never asks about, so there is nothing to suppress.
pub fn predismiss_archive(store: &mut Store, dir: &str) -> Result<bool> {
    let Some(days) = store.entry_age_days(dir)? else {
        return Ok(false);
    };
    let files = store.count_files_under(dir)?;
    record_predismissed(store, archive_question(dir, days, files))?;
    Ok(true)
}

/// Record `d` directly into the dismissed state. Any OPEN row for the same key
/// is dismissed first — "don't ask about this" must also silence the question
/// where it already surfaced (and the open row would otherwise block the
/// insert via the partial unique index). Idempotent: a dismissed row with the
/// same evidence already in place makes this a no-op.
fn record_predismissed(store: &mut Store, d: NewDecision) -> Result<()> {
    for row in store.decision_history(&d.decision_type, &d.subject)? {
        if row.status == "open" {
            store.dismiss_decision(row.id)?;
        }
    }
    if let Some(id) = store.record_decision(d)? {
        store.dismiss_decision(id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SummaryRecord;
    use crate::walker::{Entry, EntryKind};
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    /// An entries row whose mtime is ancient (epoch + 1000 s) — far past any
    /// staleness horizon, but NOT NULL (NULL = unknown, which the detector skips).
    fn old_entry(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size: 0,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1_000)),
            hint: None,
        }
    }

    fn file_summary(path: &str, source_hash: &str) -> SummaryRecord {
        SummaryRecord {
            path: path.to_owned(),
            kind: "file".into(),
            parent_path: Some("/r".to_owned()),
            depth: 1,
            summary: format!("summary of {path}"),
            summary_l0: None,
            embedding: None,
            child_count: 0,
            byte_size: 10,
            model: "test".into(),
            source_hash: source_hash.to_owned(),
            generated_at: 1,
        }
    }

    #[test]
    fn fingerprint_ignores_one_extra_file_at_coarse_rounding() {
        let a = classification_fingerprint(None, &[("code".into(), 40)]);
        // 40/41 ≈ 0.976 rounds to the same 0.05 bucket as 1.0; the stray
        // document's own share rounds to zero and is omitted.
        let b = classification_fingerprint(None, &[("code".into(), 40), ("documents".into(), 1)]);
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_changes_on_material_shift_or_hint_change() {
        let base = classification_fingerprint(None, &[("code".into(), 40)]);
        // Composition shift: half the folder is now documents.
        let shifted =
            classification_fingerprint(None, &[("code".into(), 40), ("documents".into(), 40)]);
        assert_ne!(base, shifted);
        // The dir's own hint appearing is material on its own.
        let hinted = classification_fingerprint(Some("build-artifact"), &[("code".into(), 40)]);
        assert_ne!(base, hinted);
    }

    #[test]
    fn fingerprint_is_order_independent() {
        let a = classification_fingerprint(None, &[("code".into(), 10), ("media".into(), 10)]);
        let b = classification_fingerprint(None, &[("media".into(), 10), ("code".into(), 10)]);
        assert_eq!(a, b);
    }

    #[test]
    fn run_detectors_opens_once_and_skips_covered_clusters() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&file_summary("/r/a.txt", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/b.txt", "H1"))
            .unwrap();

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (1, 0));
        let open = store.open_decisions(None, 10).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].decision_type, "duplicate");
        assert_eq!(open[0].subject, "/r/a.txt");
        let options: Vec<String> = serde_json::from_str(&open[0].options).unwrap();
        assert_eq!(options, vec!["/r/a.txt", "/r/b.txt", "keep_all"]);

        // Second pass: the open question covers both members → skipped, not duplicated.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (0, 1));

        // Answered (decided, un-superseded) still covers the cluster.
        super::super::decide_and_apply(&mut store, open[0].id, "/r/a.txt", "user").unwrap();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (0, 1));
    }

    #[test]
    fn run_detectors_expires_questions_whose_evidence_left_the_index() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&file_summary("/r/a.txt", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/b.txt", "H1"))
            .unwrap();
        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 1);

        // One member's evidence disappears entirely (no entries row existed;
        // now its summary goes too — e.g. the file was deleted and pruned).
        store.delete_summary("/r/b.txt").unwrap();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.expired, 1, "the orphaned question must expire");
        assert_eq!(store.open_decision_count().unwrap(), 0);
        // Recorded, not dropped: history shows the expired row with its note.
        let hist = store.decision_history("duplicate", "/r/a.txt").unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].status, "expired");
        assert!(hist[0].params.contains("left the index"));
    }

    #[test]
    fn archive_detector_asks_about_topmost_stale_dirs_only() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                old_entry("/old", EntryKind::Dir),
                old_entry("/old/sub", EntryKind::Dir),
                old_entry("/old/a.txt", EntryKind::File),
                old_entry("/old/sub/b.txt", EntryKind::File),
                // Shares the string prefix but is NOT under /old — must get its
                // own question (the /proj vs /projector boundary check).
                old_entry("/old-sibling", EntryKind::Dir),
            ])
            .unwrap();

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 2, "topmost dirs only, /old/sub filtered");

        let open = store.open_decisions(Some("archive"), 10).unwrap();
        let mut subjects: Vec<&str> = open.iter().map(|d| d.subject.as_str()).collect();
        subjects.sort_unstable();
        assert_eq!(subjects, vec!["/old", "/old-sibling"]);
        for d in &open {
            assert_eq!(d.priority, 30);
            let options: Vec<String> = serde_json::from_str(&d.options).unwrap();
            assert_eq!(options, vec!["archive", "keep_active"]);
        }
        let old = open.iter().find(|d| d.subject == "/old").unwrap();
        let params: serde_json::Value = serde_json::from_str(&old.params).unwrap();
        assert_eq!(params["files"], 2, "subtree file count: a.txt + sub/b.txt");
        assert!(params["days"].as_i64().unwrap() > 365);

        // Second pass: the open questions cover both dirs — nothing duplicated.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (0, 2));
    }

    #[test]
    fn archive_detector_skips_unknown_mtime_and_classified_dirs() {
        let mut store = Store::open_in_memory().unwrap();
        let mut unknown = old_entry("/mystery", EntryKind::Dir);
        unknown.modified = None; // NULL mtime = unknown, not evidence of age
        store
            .upsert_entries(&[unknown, old_entry("/archived", EntryKind::Dir)])
            .unwrap();
        store
            .confirm_classification("/archived", "archive")
            .unwrap();

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 0);
        assert_eq!(
            report.skipped, 1,
            "/archived skipped; /mystery not a candidate"
        );
    }

    #[test]
    fn archive_answer_projects_classification_and_dir_weight() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                old_entry("/old", EntryKind::Dir),
                old_entry("/old/a.txt", EntryKind::File),
            ])
            .unwrap();
        let cfg = crate::config::ReviewConfig::default();
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
        let id = store.open_decisions(Some("archive"), 1).unwrap()[0].id;

        let effects = super::super::decide_and_apply(&mut store, id, "archive", "user").unwrap();
        assert_eq!(effects["classification"], "archive");
        let c = store.classification_for("/old").unwrap().unwrap();
        assert_eq!(
            (c.category.as_str(), c.source.as_str()),
            ("archive", "user")
        );
        let w = store.list_weights(Some("dir")).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].target.as_str(), w[0].weight), ("/old", 0.5));
        assert_eq!(
            w[0].reason.as_deref(),
            Some(&*format!("decision:{id} archived"))
        );

        // Next pass: the archive classification suppresses any re-ask.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (0, 1));
    }

    #[test]
    fn keep_active_writes_no_classification_and_reasks_on_bucket_change() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                old_entry("/old", EntryKind::Dir),
                old_entry("/old/a.txt", EntryKind::File),
            ])
            .unwrap();
        let cfg = crate::config::ReviewConfig::default();
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
        let id = store.open_decisions(Some("archive"), 1).unwrap()[0].id;

        super::super::decide_and_apply(&mut store, id, "keep_active", "user").unwrap();
        assert!(store.classification_for("/old").unwrap().is_none());
        assert!(store.list_weights(Some("dir")).unwrap().is_empty());

        // Unchanged evidence: the decided keep_active row covers the dir.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (0, 1));

        // Evidence moves (file count changes; staleness buckets move the same
        // way as the dir ages) → re-ask CHAINED to the prior, never a second head.
        store
            .upsert_entries(&[old_entry("/old/b.txt", EntryKind::File)])
            .unwrap();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 1);
        let reask = &store.open_decisions(Some("archive"), 1).unwrap()[0];
        assert_eq!(
            reask.parent_id,
            Some(id),
            "re-ask chains to the prior answer"
        );

        // Resolving the re-ask supersedes the prior — exactly one live head.
        super::super::decide_and_apply(&mut store, reask.id, "archive", "user").unwrap();
        assert_eq!(
            store.decision_by_id(id).unwrap().unwrap().superseded_by,
            Some(reask.id)
        );
        let c = store.classification_for("/old").unwrap().unwrap();
        assert_eq!(c.category, "archive");
    }

    #[test]
    fn predismiss_duplicate_suppresses_the_detector() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&file_summary("/r/a.txt", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/b.txt", "H1"))
            .unwrap();

        // "Don't ask about this" arrives BEFORE the detector ever ran.
        predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
        assert_eq!(store.open_decision_count().unwrap(), 0);

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(
            report.opened, 0,
            "sticky dismissal must suppress the question"
        );
        assert_eq!(report.skipped, 1);

        // Idempotent: a second dismissal of the same evidence is a no-op.
        predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
        assert_eq!(
            store
                .decision_history("duplicate", "/r/a.txt")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn predismiss_also_dismisses_an_already_open_question() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&file_summary("/r/a.txt", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/b.txt", "H1"))
            .unwrap();
        let cfg = crate::config::ReviewConfig::default();
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);

        predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
        assert_eq!(
            store.open_decision_count().unwrap(),
            0,
            "the live question is dismissed along with the future one"
        );
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 0);
    }

    #[test]
    fn predismiss_archive_suppresses_the_detector_or_reports_nothing_to_do() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                old_entry("/old", EntryKind::Dir),
                old_entry("/old/a.txt", EntryKind::File),
            ])
            .unwrap();

        assert!(predismiss_archive(&mut store, "/old").unwrap());
        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!((report.opened, report.skipped), (0, 1));

        // Not indexed → nothing the detector would ever ask → nothing recorded.
        assert!(!predismiss_archive(&mut store, "/nope").unwrap());
        assert!(store
            .decision_history("archive", "/nope")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn gc_decisions_count_matches_what_gc_deletes() {
        // The dry-run twin `indexa prune --dry-run` relies on: count == delete.
        let mut store = Store::open_in_memory().unwrap();
        // Predismissal re-derives the cluster from the store, so the evidence
        // must actually exist (same content hash → an exact cluster).
        store
            .upsert_summary(&file_summary("/r/a.txt", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/b.txt", "H1"))
            .unwrap();
        predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
        // Horizon in the future (negative age): the fresh dismissal qualifies.
        assert_eq!(store.gc_decisions_count(-10).unwrap(), 1);
        assert_eq!(store.gc_decisions(-10).unwrap(), 1);
        assert_eq!(store.gc_decisions_count(-10).unwrap(), 0);
        // A horizon in the past keeps the (re-recorded) fresh row.
        predismiss_duplicate(&mut store, &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()]).unwrap();
        assert_eq!(store.gc_decisions_count(365 * 86_400).unwrap(), 0);
    }

    // ── Summary drift ─────────────────────────────────────────────────────────

    fn embedded_summary(path: &str, summary: &str, emb: Vec<f32>, model: &str) -> SummaryRecord {
        SummaryRecord {
            path: path.to_owned(),
            kind: "file".into(),
            parent_path: Some("/r".to_owned()),
            depth: 1,
            summary: summary.to_owned(),
            summary_l0: None,
            embedding: Some(emb),
            child_count: 0,
            byte_size: 10,
            model: model.to_owned(),
            source_hash: "H".to_owned(),
            generated_at: 1,
        }
    }

    #[test]
    fn drift_fires_below_threshold_and_skips_above_or_without_embeddings() {
        let mut store = Store::open_in_memory().unwrap();
        let old = embedded_summary("/r/f.txt", "Old summary. More.", vec![1.0, 0.0], "m1");
        let new = embedded_summary("/r/f.txt", "New summary. Else.", vec![0.0, 1.0], "m2");

        // Orthogonal embeddings → cosine 0 → question.
        let id = flag_summary_drift(&mut store, &old, &new).unwrap().unwrap();
        let d = store.decision_by_id(id).unwrap().unwrap();
        assert_eq!(d.decision_type, "summary_drift");
        assert_eq!(d.subject, "/r/f.txt");
        assert_eq!(d.priority, 40);
        let options: Vec<String> = serde_json::from_str(&d.options).unwrap();
        assert_eq!(options, vec!["keep_new", "restore_old"]);
        let params: serde_json::Value = serde_json::from_str(&d.params).unwrap();
        assert_eq!(params["old_summary"], "Old summary. More.");
        assert_eq!(params["old_l0"], "Old summary.");
        assert_eq!(params["new_l0"], "New summary.");
        assert_eq!(params["old_model"], "m1");
        assert_eq!(params["new_model"], "m2");
        assert!(params["cosine"].as_f64().unwrap() < 0.8);

        // Open row dedups a second fire.
        assert!(flag_summary_drift(&mut store, &old, &new)
            .unwrap()
            .is_none());

        // Near-identical embeddings → no question.
        let mut store2 = Store::open_in_memory().unwrap();
        let similar = embedded_summary("/r/f.txt", "New.", vec![0.99, 0.05], "m2");
        assert!(flag_summary_drift(&mut store2, &old, &similar)
            .unwrap()
            .is_none());

        // A missing embedding on either side skips silently.
        let mut no_emb = old.clone();
        no_emb.embedding = None;
        assert!(flag_summary_drift(&mut store2, &no_emb, &new)
            .unwrap()
            .is_none());
    }

    #[test]
    fn drift_skips_when_the_user_already_chose_for_this_evidence() {
        let mut store = Store::open_in_memory().unwrap();
        let old = embedded_summary("/r/f.txt", "Old.", vec![1.0, 0.0], "m1");
        let new = embedded_summary("/r/f.txt", "New.", vec![0.0, 1.0], "m2");
        let id = flag_summary_drift(&mut store, &old, &new).unwrap().unwrap();
        super::super::decide_and_apply(&mut store, id, "keep_new", "user").unwrap();

        // Same content + same model → the standing answer covers it.
        assert!(flag_summary_drift(&mut store, &old, &new)
            .unwrap()
            .is_none());

        // A different model is new evidence → chained re-ask, never a second head.
        let mut new2 = new.clone();
        new2.model = "m3".into();
        let reask = flag_summary_drift(&mut store, &old, &new2)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.decision_by_id(reask).unwrap().unwrap().parent_id,
            Some(id)
        );
    }

    // ── Language fallback ─────────────────────────────────────────────────────

    fn null_lang_chunks(path: &str, n: usize) -> Vec<crate::store::ChunkRecord> {
        (0..n)
            .map(|i| crate::store::ChunkRecord {
                entry_path: path.to_owned(),
                seq: i,
                heading: String::new(),
                text: format!("chunk {i}"),
                language: None,
                embedding: None,
                embed_model: None,
            })
            .collect()
    }

    /// A fresh entries row (recent mtime so the archive detector stays quiet).
    fn fresh_entry(path: &str, kind: EntryKind) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            size: 0,
            modified: Some(std::time::SystemTime::now()),
            hint: None,
        }
    }

    #[test]
    fn language_detector_asks_for_untagged_code_files_only() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                fresh_entry("/r/script.rb", EntryKind::File),
                fresh_entry("/r/notes.txt", EntryKind::File),
                fresh_entry("/r/tiny.php", EntryKind::File),
            ])
            .unwrap();
        store
            .upsert_chunks(&null_lang_chunks("/r/script.rb", 3))
            .unwrap();
        // Plain text: untagged is correct — never a question.
        store
            .upsert_chunks(&null_lang_chunks("/r/notes.txt", 5))
            .unwrap();
        // Code, but below the chunk floor — not worth an interruption.
        store
            .upsert_chunks(&null_lang_chunks("/r/tiny.php", 2))
            .unwrap();

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 1);
        let open = store.open_decisions(Some("language"), 10).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].subject, "/r/script.rb");
        assert_eq!(open[0].priority, 20);
        let options: Vec<String> = serde_json::from_str(&open[0].options).unwrap();
        // The file doesn't exist on disk → no hyperpolyglot candidate.
        assert_eq!(options, vec!["ruby", "ignore"]);
        let params: serde_json::Value = serde_json::from_str(&open[0].params).unwrap();
        assert_eq!(params["chunks"], 3);

        // Second pass: the open question dedups, nothing new.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 0);
    }

    #[test]
    fn language_answer_tags_chunks_and_is_silently_reapplied_after_rechunk() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[fresh_entry("/r/script.rb", EntryKind::File)])
            .unwrap();
        store
            .upsert_chunks(&null_lang_chunks("/r/script.rb", 3))
            .unwrap();
        let cfg = crate::config::ReviewConfig::default();
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
        let id = store.open_decisions(Some("language"), 1).unwrap()[0].id;

        let fx = super::super::decide_and_apply(&mut store, id, "ruby", "user").unwrap();
        assert_eq!(fx, serde_json::json!({"language": "ruby", "chunks": 3}));
        assert!(store.unlabeled_chunk_files(1, 10).unwrap().is_empty());

        // A re-deep rewrites the chunks untagged — the standing answer is
        // re-applied silently instead of re-asking.
        store
            .upsert_chunks(&null_lang_chunks("/r/script.rb", 4))
            .unwrap();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 0);
        assert!(store.unlabeled_chunk_files(1, 10).unwrap().is_empty());
    }

    // ── Symbol ambiguity ──────────────────────────────────────────────────────

    fn edge(from: &str, kind: &str, to: &str) -> crate::store::EdgeRecord {
        crate::store::EdgeRecord {
            from_path: from.to_owned(),
            kind: kind.to_owned(),
            to_ref: to.to_owned(),
        }
    }

    /// Seed `foo` defined in two files with one caller; entries rows keep the
    /// expiry sweep satisfied (params.paths = the definers).
    fn seed_ambiguous_foo(store: &mut Store) {
        store
            .upsert_entries(&[
                fresh_entry("/a.rs", EntryKind::File),
                fresh_entry("/b.rs", EntryKind::File),
                fresh_entry("/c.rs", EntryKind::File),
            ])
            .unwrap();
        store
            .upsert_edges(&[
                edge("/a.rs", "defines", "foo"),
                edge("/b.rs", "defines", "foo"),
                edge("/c.rs", "calls", "foo"),
            ])
            .unwrap();
    }

    #[test]
    fn symbol_detector_asks_once_and_projects_the_choice() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                fresh_entry("/a.rs", EntryKind::File),
                fresh_entry("/b.rs", EntryKind::File),
                fresh_entry("/c.rs", EntryKind::File),
            ])
            .unwrap();
        // One batch (upsert_edges replaces by from_path): `foo` is ambiguous,
        // `bar` (one definition) must NOT fire.
        store
            .upsert_edges(&[
                edge("/a.rs", "defines", "foo"),
                edge("/b.rs", "defines", "foo"),
                edge("/c.rs", "calls", "foo"),
                edge("/a.rs", "defines", "bar"),
                edge("/c.rs", "calls", "bar"),
            ])
            .unwrap();

        // symbol_ambiguity is opt-in as of v0.39 — enable it for this detector's tests.
        let cfg = crate::config::ReviewConfig {
            symbol_ambiguity: true,
            ..crate::config::ReviewConfig::default()
        };
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 1);
        let open = store.open_decisions(Some("symbol_ambiguity"), 10).unwrap();
        assert_eq!(open.len(), 1);
        let d = &open[0];
        assert_eq!(d.subject, "foo");
        assert_eq!(d.priority, 20);
        let options: Vec<String> = serde_json::from_str(&d.options).unwrap();
        assert_eq!(options, vec!["/a.rs", "/b.rs", "all"]);
        let params: serde_json::Value = serde_json::from_str(&d.params).unwrap();
        assert_eq!(params["definers"], serde_json::json!(["/a.rs", "/b.rs"]));
        assert_eq!(params["callers"], 1);
        // paths carries the definers so the expiry sweep checks files, not the
        // bare symbol name.
        assert_eq!(params["paths"], serde_json::json!(["/a.rs", "/b.rs"]));

        // Open row dedups the second pass.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 0);

        // Answering stores the choice as effects only — no domain-table writes.
        let fx = super::super::decide_and_apply(&mut store, d.id, "/a.rs", "user").unwrap();
        assert_eq!(fx, serde_json::json!({"authoritative": "/a.rs"}));

        // Decided + unchanged definer set → skipped, not re-asked.
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 0);
    }

    #[test]
    fn symbol_detector_reasks_chained_when_the_definer_set_changes() {
        let mut store = Store::open_in_memory().unwrap();
        seed_ambiguous_foo(&mut store);
        let cfg = crate::config::ReviewConfig {
            symbol_ambiguity: true,
            ..crate::config::ReviewConfig::default()
        };
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 1);
        let id = store.open_decisions(Some("symbol_ambiguity"), 1).unwrap()[0].id;
        super::super::decide_and_apply(&mut store, id, "all", "user").unwrap();
        assert_eq!(
            store
                .decision_by_id(id)
                .unwrap()
                .unwrap()
                .effects
                .as_deref(),
            Some(r#"{"authoritative":null}"#)
        );

        // A third definition appears → new evidence → chained re-ask.
        // (upsert_edges replaces /c.rs's rows — keep its existing call edge.)
        store
            .upsert_edges(&[
                edge("/c.rs", "calls", "foo"),
                edge("/c.rs", "defines", "foo"),
            ])
            .unwrap();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 1);
        let reask = &store.open_decisions(Some("symbol_ambiguity"), 1).unwrap()[0];
        assert_eq!(reask.parent_id, Some(id), "re-ask chains to the prior");
        let options: Vec<String> = serde_json::from_str(&reask.options).unwrap();
        assert_eq!(options, vec!["/a.rs", "/b.rs", "/c.rs", "all"]);
    }

    #[test]
    fn symbol_detector_honors_top_k_and_scan_caps() {
        let mut store = Store::open_in_memory().unwrap();
        // 12 ambiguous symbols, each defined twice and called once.
        let mut edges = Vec::new();
        let mut entries = Vec::new();
        for i in 0..12 {
            let (a, b, c) = (
                format!("/a{i}.rs"),
                format!("/b{i}.rs"),
                format!("/c{i}.rs"),
            );
            for p in [&a, &b, &c] {
                entries.push(fresh_entry(p, EntryKind::File));
            }
            let sym = format!("sym{i}");
            edges.push(edge(&a, "defines", &sym));
            edges.push(edge(&b, "defines", &sym));
            edges.push(edge(&c, "calls", &sym));
        }
        store.upsert_entries(&entries).unwrap();
        store.upsert_edges(&edges).unwrap();

        // max_new_per_scan below top-K: the scan cap wins.
        let cfg = crate::config::ReviewConfig {
            max_new_per_scan: 4,
            symbol_ambiguity: true,
            ..crate::config::ReviewConfig::default()
        };
        assert_eq!(run_detectors(&mut store, &cfg).unwrap().opened, 4);

        // With a generous cap the per-scan top-K (10) bounds the rest:
        // 6 remaining of the K=10 hottest open on the second pass.
        let cfg = crate::config::ReviewConfig {
            symbol_ambiguity: true,
            ..crate::config::ReviewConfig::default()
        };
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(
            report.opened + 4,
            10,
            "top-K bounds the per-scan candidates"
        );
    }

    #[test]
    fn run_detectors_honors_caps() {
        let mut store = Store::open_in_memory().unwrap();
        for i in 0..3 {
            store
                .upsert_summary(&file_summary(&format!("/r/a{i}.txt"), &format!("H{i}")))
                .unwrap();
            store
                .upsert_summary(&file_summary(&format!("/r/b{i}.txt"), &format!("H{i}")))
                .unwrap();
        }
        let cfg = crate::config::ReviewConfig {
            max_new_per_scan: 2,
            ..crate::config::ReviewConfig::default()
        };
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(report.opened, 2);
        assert_eq!(store.open_decision_count().unwrap(), 2);
    }

    // ── v0.39 noise filters ─────────────────────────────────────────────────────
    #[test]
    fn duplicate_detector_skips_asset_clusters_keeps_source() {
        let mut store = Store::open_in_memory().unwrap();
        // Identical-content images (exact dup) — assets, NOT actionable.
        store
            .upsert_summary(&file_summary("/r/x.png", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/y.png", "H1"))
            .unwrap();
        // Identical-content source — a real, actionable duplicate.
        store
            .upsert_summary(&file_summary("/r/a.rs", "H2"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/b.rs", "H2"))
            .unwrap();

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(
            report.opened, 1,
            "only the source cluster opens; the image cluster is filtered"
        );
        let open = store.open_decisions(Some("duplicate"), 10).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].subject, "/r/a.rs");
    }

    #[test]
    fn duplicate_detector_skips_generated_dir_clusters() {
        let mut store = Store::open_in_memory().unwrap();
        // Source files (not asset-extension) but under a generated icon tree — not actionable.
        store
            .upsert_summary(&file_summary("/r/icons/ios/Icon-1.txt", "H1"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/icons/ios/Icon-2.txt", "H1"))
            .unwrap();
        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(
            report.opened, 0,
            "a generated/asset tree is never a dedupe target"
        );
    }

    #[test]
    fn symbol_ambiguity_is_off_by_default() {
        let mut store = Store::open_in_memory().unwrap();
        seed_ambiguous_foo(&mut store);
        let cfg = crate::config::ReviewConfig::default(); // symbol_ambiguity = false
        run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(
            store
                .open_decisions(Some("symbol_ambiguity"), 10)
                .unwrap()
                .len(),
            0,
            "the unanswerable-question detector must stay quiet unless opted in"
        );
    }

    #[test]
    fn symbol_ambiguity_skips_idioms_even_when_enabled() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                fresh_entry("/a.rs", EntryKind::File),
                fresh_entry("/b.rs", EntryKind::File),
                fresh_entry("/c.rs", EntryKind::File),
            ])
            .unwrap();
        // `new` is a universal idiom: defined in two files, called once. Even with the
        // feature ON, asking "which `new` is authoritative?" is noise — must not fire.
        store
            .upsert_edges(&[
                edge("/a.rs", "defines", "new"),
                edge("/b.rs", "defines", "new"),
                edge("/c.rs", "calls", "new"),
            ])
            .unwrap();
        let cfg = crate::config::ReviewConfig {
            symbol_ambiguity: true,
            ..crate::config::ReviewConfig::default()
        };
        run_detectors(&mut store, &cfg).unwrap();
        assert_eq!(
            store
                .open_decisions(Some("symbol_ambiguity"), 10)
                .unwrap()
                .len(),
            0,
            "idiom `new` must not surface even with the feature on"
        );
    }

    #[test]
    fn sweep_dismisses_disabled_symbol_ambiguity_rows() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_entries(&[
                fresh_entry("/a.rs", EntryKind::File),
                fresh_entry("/b.rs", EntryKind::File),
            ])
            .unwrap();
        // Pre-existing ambiguity row (e.g. created before v0.39). record_decision does
        // not filter — the detector/sweep do.
        store
            .record_decision(symbol_ambiguity_question(
                "resolve_base_url",
                &["/a.rs".to_owned(), "/b.rs".to_owned()],
                2,
            ))
            .unwrap();
        assert_eq!(store.open_decision_count().unwrap(), 1);

        let cfg = crate::config::ReviewConfig::default(); // feature off
        assert_eq!(
            sweep_filtered_noise(&mut store, &cfg, true).unwrap(),
            1,
            "dry-run counts it"
        );
        assert_eq!(
            store.open_decision_count().unwrap(),
            1,
            "dry-run dismisses nothing"
        );
        assert_eq!(sweep_filtered_noise(&mut store, &cfg, false).unwrap(), 1);
        assert_eq!(
            store.open_decision_count().unwrap(),
            0,
            "sweep dismissed the disabled-feature row"
        );
    }

    #[test]
    fn idiom_and_actionable_helpers() {
        assert!(is_idiom_symbol("new"));
        assert!(is_idiom_symbol("Default")); // case-insensitive
        assert!(is_idiom_symbol("with_timeout"));
        assert!(is_idiom_symbol("set_scope"));
        assert!(!is_idiom_symbol("resolve_base_url"));
        assert!(!is_idiom_symbol("compute_budget"));
        assert!(!duplicate_cluster_actionable(&[
            "/a/x.webp".into(),
            "/a/y.webp".into()
        ]));
        assert!(!duplicate_cluster_actionable(&[
            "/p/icons/a.rs".into(),
            "/p/b.rs".into()
        ]));
        assert!(duplicate_cluster_actionable(&[
            "/p/a.rs".into(),
            "/p/b.rs".into()
        ]));
    }

    // ── v0.40 near-dup basename filter tests ─────────────────────────────────

    #[test]
    fn near_dup_same_basenames_helper() {
        // Same basename in different dirs → true (potentially a copy).
        assert!(near_dup_same_basenames(&[
            "/crates/query/src/qa.rs".into(),
            "/crates/web/src/qa.rs".into(),
        ]));
        // Different basenames → false (likely a false positive).
        assert!(!near_dup_same_basenames(&[
            "/crates/query/src/summarize.rs".into(),
            "/crates/web/src/jobs_exec.rs".into(),
        ]));
        // Three members, all same → true.
        assert!(near_dup_same_basenames(&[
            "/a/foo.rs".into(),
            "/b/foo.rs".into(),
            "/c/foo.rs".into(),
        ]));
        // Three members, one different → false.
        assert!(!near_dup_same_basenames(&[
            "/a/foo.rs".into(),
            "/b/foo.rs".into(),
            "/c/bar.rs".into(),
        ]));
        // Single member → true (vacuous; no question is opened for single-member clusters).
        assert!(near_dup_same_basenames(&["/a/foo.rs".into()]));
        // Empty → true (vacuous).
        assert!(near_dup_same_basenames(&[]));
    }

    #[test]
    fn near_dup_differently_named_cluster_is_skipped() {
        // Two files with different names get a high-similarity embedding cluster
        // (via same source_hash used as embedding proxy in find_exact_duplicates).
        // A near-dup of differently-named files must be skipped (not opened).
        let mut store = Store::open_in_memory().unwrap();
        // Use different source hashes so find_exact_duplicates doesn't fire;
        // the question is whether the near-dup path is blocked by name check.
        // We seed the open decision directly to test sweep_filtered_noise.
        let q = NewDecision {
            decision_type: "duplicate".into(),
            subject: "/r/summarize.rs".into(),
            params: serde_json::json!({
                "paths": ["/r/summarize.rs", "/r/jobs_exec.rs"],
                "similarity": 0.97_f32,
                "exact": false,
            }),
            options: serde_json::json!(["/r/summarize.rs", "/r/jobs_exec.rs", "keep_all"]),
            auto_value: Some("/r/summarize.rs".into()),
            confidence: Some(0.97),
            evidence_hash: "test-hash-near-diff-name".into(),
            priority: 60,
            paths: vec!["/r/summarize.rs".into(), "/r/jobs_exec.rs".into()],
        };
        store.record_decision(q).unwrap();
        assert_eq!(store.open_decision_count().unwrap(), 1);

        // sweep_filtered_noise should dismiss it.
        let cfg = crate::config::ReviewConfig::default();
        let n = sweep_filtered_noise(&mut store, &cfg, false).unwrap();
        assert_eq!(n, 1, "differently-named near-dup must be swept");
        assert_eq!(store.open_decision_count().unwrap(), 0);
    }

    #[test]
    fn near_dup_same_named_cluster_is_kept() {
        // Same basename (qa.rs in two crates) is a real copy candidate — keep asking.
        let mut store = Store::open_in_memory().unwrap();
        let q = NewDecision {
            decision_type: "duplicate".into(),
            subject: "/crates/query/src/qa.rs".into(),
            params: serde_json::json!({
                "paths": ["/crates/query/src/qa.rs", "/crates/web/src/qa.rs"],
                "similarity": 0.97_f32,
                "exact": false,
            }),
            options: serde_json::json!([
                "/crates/query/src/qa.rs",
                "/crates/web/src/qa.rs",
                "keep_all"
            ]),
            auto_value: Some("/crates/query/src/qa.rs".into()),
            confidence: Some(0.97),
            evidence_hash: "test-hash-near-same-name".into(),
            priority: 60,
            paths: vec![
                "/crates/query/src/qa.rs".into(),
                "/crates/web/src/qa.rs".into(),
            ],
        };
        store.record_decision(q).unwrap();
        assert_eq!(store.open_decision_count().unwrap(), 1);

        let cfg = crate::config::ReviewConfig::default();
        let n = sweep_filtered_noise(&mut store, &cfg, false).unwrap();
        assert_eq!(n, 0, "same-basename near-dup must NOT be swept");
        assert_eq!(
            store.open_decision_count().unwrap(),
            1,
            "question must remain open"
        );
    }

    #[test]
    fn exact_dup_differently_named_is_kept() {
        // Exact content (similarity 1.0) — always ask regardless of name.
        let mut store = Store::open_in_memory().unwrap();
        let q = NewDecision {
            decision_type: "duplicate".into(),
            subject: "/r/alpha.rs".into(),
            params: serde_json::json!({
                "paths": ["/r/alpha.rs", "/r/beta.rs"],
                "similarity": 1.0_f32,
                "exact": true,
            }),
            options: serde_json::json!(["/r/alpha.rs", "/r/beta.rs", "keep_all"]),
            auto_value: Some("/r/alpha.rs".into()),
            confidence: Some(1.0),
            evidence_hash: "test-hash-exact-diff-name".into(),
            priority: 60,
            paths: vec!["/r/alpha.rs".into(), "/r/beta.rs".into()],
        };
        store.record_decision(q).unwrap();
        assert_eq!(store.open_decision_count().unwrap(), 1);

        let cfg = crate::config::ReviewConfig::default();
        let n = sweep_filtered_noise(&mut store, &cfg, false).unwrap();
        assert_eq!(
            n, 0,
            "exact-content dup must NOT be swept even if names differ"
        );
        assert_eq!(store.open_decision_count().unwrap(), 1);
    }

    #[test]
    fn run_detectors_skips_near_dup_differently_named_cluster() {
        // Integration test: run_detectors must skip the cluster before opening it.
        // We seed two files with the same source_hash so find_exact_duplicates fires
        // but differently-named files — exact=true so the basename gate does NOT apply,
        // confirming that exact clusters still go through. Then we also test near-dup
        // via the seeding loop gate (exact=false, different names must be skipped).
        //
        // The store's find_near_duplicates requires embeddings, so we test the seeding
        // gate indirectly: inject a near-dup differently-named open question, then run
        // run_detectors which calls sweep_filtered_noise first and clears it.
        let mut store = Store::open_in_memory().unwrap();
        let q = NewDecision {
            decision_type: "duplicate".into(),
            subject: "/r/graph.rs".into(),
            params: serde_json::json!({
                "paths": ["/r/graph.rs", "/r/pack.rs"],
                "similarity": 0.96_f32,
                "exact": false,
            }),
            options: serde_json::json!(["/r/graph.rs", "/r/pack.rs", "keep_all"]),
            auto_value: Some("/r/graph.rs".into()),
            confidence: Some(0.96),
            evidence_hash: "test-hash-gate-integration".into(),
            priority: 60,
            paths: vec!["/r/graph.rs".into(), "/r/pack.rs".into()],
        };
        store.record_decision(q).unwrap();
        assert_eq!(store.open_decision_count().unwrap(), 1);

        // Summaries for the members are needed so the expiry sweep doesn't expire them.
        store
            .upsert_summary(&file_summary("/r/graph.rs", "Hg"))
            .unwrap();
        store
            .upsert_summary(&file_summary("/r/pack.rs", "Hp"))
            .unwrap();

        let cfg = crate::config::ReviewConfig::default();
        let report = run_detectors(&mut store, &cfg).unwrap();
        // sweep_filtered_noise counts as skipped; the question must be gone.
        assert!(
            report.skipped >= 1,
            "differently-named near-dup must be swept by run_detectors"
        );
        assert_eq!(
            store.open_decision_count().unwrap(),
            0,
            "question must be dismissed"
        );
    }
}
