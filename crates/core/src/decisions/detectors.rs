//! Detectors: turn the uncertainty signals the pipeline already produces
//! (duplicate clusters, mid-band Tier-0 confidence) into open ledger questions.
//!
//! The classification detectors fire inline in `cmd_classify` (they need the
//! Tier-0 hint maps that only exist there); [`run_detectors`] is the standalone
//! pass appended to `cmd_index` and covers the duplicate and archive detectors
//! plus the crash-repair and expiry sweeps.

use crate::config::ReviewConfig;
use crate::store::{DuplicateCluster, NewDecision, Store};
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

/// What a detector pass did. Totals are across detector types (duplicate +
/// archive); a per-type split waits until a surface actually needs it.
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
/// duplicate and archive detectors, honoring the fatigue caps in `cfg`.
pub fn run_detectors(store: &mut Store, cfg: &ReviewConfig) -> Result<DetectorReport> {
    let mut report = DetectorReport {
        repaired: super::repair_unapplied(store)?,
        ..DetectorReport::default()
    };

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
}
