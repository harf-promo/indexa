//! Smart (semantic) classification — Tier 0: deterministic and content-free.
//!
//! A **second axis** over the technical `hint_cat` ([`crate::surface`]). Every
//! directory can carry a semantic category — work / personal / archive / media /
//! code / system / other — that the user confirms, corrects, or ignores.
//!
//! Tier 0 derives the *content-free* half (code/media/system/archive) for free
//! from surface hints alone. The work/personal half cannot come from path shape
//! or file type — only folder content or an explicit user declaration produces
//! it — and is handled by later tiers (embedding cosine / LLM roll-up). So a
//! directory Tier 0 cannot place is left **unclassified** (awaiting inference),
//! never guessed.

use std::fmt;

/// A semantic category on the Smart-classification axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticCategory {
    Work,
    Personal,
    Archive,
    Media,
    Code,
    System,
    Other,
}

impl SemanticCategory {
    /// The stable wire/string form (used in the DB, CLI, and API).
    pub fn as_str(self) -> &'static str {
        match self {
            SemanticCategory::Work => "work",
            SemanticCategory::Personal => "personal",
            SemanticCategory::Archive => "archive",
            SemanticCategory::Media => "media",
            SemanticCategory::Code => "code",
            SemanticCategory::System => "system",
            SemanticCategory::Other => "other",
        }
    }

    /// Parse a wire string back into a category (for validating a user correction).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "work" => SemanticCategory::Work,
            "personal" => SemanticCategory::Personal,
            "archive" => SemanticCategory::Archive,
            "media" => SemanticCategory::Media,
            "code" => SemanticCategory::Code,
            "system" => SemanticCategory::System,
            "other" => SemanticCategory::Other,
            _ => return None,
        })
    }

    /// Every category, for UI dropdowns and `--category` validation.
    pub const ALL: [SemanticCategory; 7] = [
        SemanticCategory::Work,
        SemanticCategory::Personal,
        SemanticCategory::Archive,
        SemanticCategory::Media,
        SemanticCategory::Code,
        SemanticCategory::System,
        SemanticCategory::Other,
    ];
}

impl fmt::Display for SemanticCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Confidence stamped on a Tier 0 match made from a directory's *own* surface hint
/// (e.g. `node_modules` → code). Higher than aggregation, which is probabilistic.
pub const TIER0_OWN_HINT_CONFIDENCE: f32 = 0.9;

/// Minimum share of a directory's *direct* children that must agree before
/// aggregation assigns a category — so a folder with 2 code files among 100
/// documents is not called "code".
pub const TIER0_AGGREGATION_THRESHOLD: f32 = 0.6;

/// Tier 0 map: a technical `hint_cat` → a content-free semantic category.
///
/// Returns `None` for categories that need content inference to split into
/// work/personal (`documents`, `data`, `config`, `scratch`, `logs`) and for
/// unknown hints.
pub fn semantic_from_hint_cat(hint_cat: &str) -> Option<SemanticCategory> {
    Some(match hint_cat {
        "code" | "build-artifact" | "lockfile" => SemanticCategory::Code,
        "media" | "font" => SemanticCategory::Media,
        "system" | "cache" | "apps" | "applications" => SemanticCategory::System,
        "archive" => SemanticCategory::Archive,
        _ => return None,
    })
}

/// Tier 0 classify one directory, content-free. Priority:
///
/// 1. The directory's **own** surface hint, if it maps (confidence
///    [`TIER0_OWN_HINT_CONFIDENCE`]).
/// 2. Otherwise the **dominant** semantic category among its direct child files,
///    if a clear majority ([`TIER0_AGGREGATION_THRESHOLD`]) of *all* direct
///    children agree (confidence = that share).
///
/// Returns `None` when neither yields a category — the directory awaits a later
/// (content-based) tier and is shown as pending rather than guessed.
///
/// `child_hint_counts`: `(hint_cat, count)` over the directory's direct child files.
pub fn classify_dir_tier0(
    own_hint_cat: Option<&str>,
    child_hint_counts: &[(String, i64)],
) -> Option<(SemanticCategory, f32)> {
    if let Some(cat) = own_hint_cat.and_then(semantic_from_hint_cat) {
        return Some((cat, TIER0_OWN_HINT_CONFIDENCE));
    }

    // Aggregate direct child files by semantic category. The denominator is *all*
    // direct children (classifiable or not), so a low-signal folder stays pending.
    let mut total: i64 = 0;
    let mut tally: Vec<(SemanticCategory, i64)> = Vec::new();
    for (hint_cat, n) in child_hint_counts {
        total += *n;
        if let Some(cat) = semantic_from_hint_cat(hint_cat) {
            match tally.iter_mut().find(|(c, _)| *c == cat) {
                Some(entry) => entry.1 += *n,
                None => tally.push((cat, *n)),
            }
        }
    }
    if total == 0 {
        return None;
    }
    let (best_cat, best_n) = tally.into_iter().max_by_key(|(_, n)| *n)?;
    let share = best_n as f32 / total as f32;
    if share >= TIER0_AGGREGATION_THRESHOLD {
        Some((best_cat, share))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_roundtrip_is_stable() {
        for c in SemanticCategory::ALL {
            assert_eq!(SemanticCategory::parse(c.as_str()), Some(c));
        }
        assert_eq!(SemanticCategory::parse("nonsense"), None);
    }

    #[test]
    fn hint_cat_maps_content_free_half() {
        assert_eq!(semantic_from_hint_cat("code"), Some(SemanticCategory::Code));
        assert_eq!(
            semantic_from_hint_cat("build-artifact"),
            Some(SemanticCategory::Code)
        );
        assert_eq!(
            semantic_from_hint_cat("lockfile"),
            Some(SemanticCategory::Code)
        );
        assert_eq!(
            semantic_from_hint_cat("media"),
            Some(SemanticCategory::Media)
        );
        assert_eq!(
            semantic_from_hint_cat("font"),
            Some(SemanticCategory::Media)
        );
        assert_eq!(
            semantic_from_hint_cat("cache"),
            Some(SemanticCategory::System)
        );
        assert_eq!(
            semantic_from_hint_cat("applications"),
            Some(SemanticCategory::System)
        );
        assert_eq!(
            semantic_from_hint_cat("archive"),
            Some(SemanticCategory::Archive)
        );
    }

    #[test]
    fn work_personal_are_never_guessed_by_tier0() {
        // Categories that need content inference must not map deterministically.
        for hc in ["documents", "data", "config", "scratch", "logs", "unknown"] {
            assert_eq!(semantic_from_hint_cat(hc), None, "{hc} should be unmapped");
        }
    }

    #[test]
    fn own_hint_takes_priority_over_children() {
        // A node_modules-style dir keeps its own hint even with no/other children.
        let got = classify_dir_tier0(Some("build-artifact"), &[("documents".into(), 50)]);
        assert_eq!(
            got,
            Some((SemanticCategory::Code, TIER0_OWN_HINT_CONFIDENCE))
        );
    }

    #[test]
    fn dominant_children_classify_a_hintless_dir() {
        // A code project folder with no own hint but mostly code files → code.
        let children = vec![("code".into(), 18), ("config".into(), 2)];
        let got = classify_dir_tier0(None, &children).unwrap();
        assert_eq!(got.0, SemanticCategory::Code);
        assert!(
            (got.1 - 0.9).abs() < 1e-6,
            "share 18/20 = 0.9, got {}",
            got.1
        );
    }

    #[test]
    fn weak_majority_stays_pending() {
        // 2 code files among 100 docs → not enough to call it code.
        let children = vec![("code".into(), 2), ("documents".into(), 98)];
        assert_eq!(classify_dir_tier0(None, &children), None);
    }

    #[test]
    fn purely_uninferable_children_stay_pending() {
        let children = vec![("documents".into(), 10), ("data".into(), 5)];
        assert_eq!(classify_dir_tier0(None, &children), None);
    }

    #[test]
    fn empty_inputs_yield_nothing() {
        assert_eq!(classify_dir_tier0(None, &[]), None);
        assert_eq!(classify_dir_tier0(Some("documents"), &[]), None);
    }

    #[test]
    fn media_folder_classifies_by_children() {
        let children = vec![("media".into(), 40), ("font".into(), 5)];
        let got = classify_dir_tier0(None, &children).unwrap();
        assert_eq!(got.0, SemanticCategory::Media);
        assert!(got.1 >= TIER0_AGGREGATION_THRESHOLD);
    }
}
