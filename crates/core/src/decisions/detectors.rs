//! Detectors: turn the uncertainty signals the pipeline already produces
//! (duplicate clusters, mid-band Tier-0 confidence) into open ledger questions.
//!
//! The classification detectors fire inline in `cmd_classify` (they need the
//! Tier-0 hint maps that only exist there); [`run_detectors`] is the standalone
//! pass appended to `cmd_index` and covers the duplicate detector plus the
//! crash-repair sweep.

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

/// What a detector pass did. Phase 1 runs a single detector type here
/// (duplicate); the totals gain a per-type split when more detectors join.
#[derive(Debug, Default, Clone, Copy)]
pub struct DetectorReport {
    /// Questions opened this pass.
    pub opened: usize,
    /// Clusters skipped: already covered by a live decision, deduped against an
    /// existing open question, or sticky-dismissed with unchanged evidence.
    pub skipped: usize,
    /// Decided rows whose projection was re-run by the crash-repair sweep.
    pub repaired: usize,
}

/// The detector pass run at the end of `cmd_index`: repair sweep first (so a
/// crashed projection heals before new questions stack on top), then the
/// duplicate detector, honoring the fatigue caps in `cfg`.
pub fn run_detectors(store: &mut Store, cfg: &ReviewConfig) -> Result<DetectorReport> {
    let mut report = DetectorReport {
        repaired: super::repair_unapplied(store)?,
        ..DetectorReport::default()
    };

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
    Ok(report)
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

/// Duplicate-cluster evidence fingerprint: sorted member paths, the exact flag,
/// and similarity rounded to 0.01. A dismissed cluster question only returns
/// when membership changes or similarity moves by a visible amount.
fn duplicate_fingerprint(sorted_paths: &[String], exact: bool, similarity: f32) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SummaryRecord;

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
