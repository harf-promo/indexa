//! The `RECORDED MEMORY` block: durable claims offered to `ask` as labelled background.
//!
//! # Why a block and not a retrieval arm
//!
//! Two alternatives were rejected, both for concrete reasons rather than taste.
//!
//! **Memories as chunks** would need a synthetic `entries.path`, which puts non-file content in
//! the table `deep`, `prune`, the coverage treemap and the savings accounting all assume is
//! file-backed. A memory a `prune` can delete is not a memory.
//!
//! **Memories merged into the fused hit list** would make `SearchHit` represent something that
//! is not a chunk, leaking into MMR's `embeddings_for_chunks`, the global `[1..N]` citation
//! counter, `SourceCitation.path`, per-citation staleness, and impact accounting — five places
//! a claim could silently displace a real source excerpt.
//!
//! And the decisive one: **a trust boundary cannot be drawn around a claim that has already been
//! given a citation number next to real file content.** A model reading `[3]` has no way to tell
//! that one of those numbers is somebody's hypothesis. The block exists so that distinction can
//! be stated in words, right next to the material it distinguishes itself from.
//!
//! # Budget
//!
//! The block is appended to the project-overview string, which `pack_context_clustered` counts
//! against the same `chunk_budget` as everything else. So enabling memory **trades source bytes
//! for claim bytes** — it never grows the prompt.

use indexa_core::config::MemoryConfig;
use indexa_core::memory::{MemoryKind, MemoryRecord};
use indexa_core::store::{SearchHit, Store};

/// Header text. Long on purpose: it is the only place the model is told that these lines are
/// not retrieved file content, that the kinds differ in how far they should be trusted, and
/// that a memory line must never be cited as a source.
const HEADER: &str = "RECORDED MEMORY (claims recorded by the operator or an agent — NOT \
retrieved file content. Treat `observed` and `stated` as reliable; treat `inferred` and \
`hypothesis` as leads to verify, not facts. Cite the numbered excerpts below, never a memory \
line):";

/// Build the block, or an empty string when it is off, empty, or has no room.
///
/// `budget` is a **byte** budget, matching `build_project_overview`'s convention (the packer
/// measures with `String::len`). Truncation happens at an item boundary, never mid-claim: half
/// a claim reads as a different claim.
pub(crate) fn build_memory_block(
    store: &Store,
    cfg: &MemoryConfig,
    hits: &[SearchHit],
    budget: usize,
) -> String {
    if !cfg.retrieval || budget == 0 || cfg.max_items == 0 {
        return String::new();
    }
    let kinds: Vec<MemoryKind> = cfg
        .include_kinds
        .iter()
        .filter_map(|k| MemoryKind::parse(k))
        .collect();
    if kinds.is_empty() {
        return String::new();
    }

    // Prefer claims about the files this answer is actually drawing on; fall back to the
    // highest-trust claims overall. Both paths go through the store's own liveness filtering,
    // so a failed, retired or expired claim can never reach a prompt.
    let mut candidates = {
        let paths: Vec<String> = hits
            .iter()
            .take(5)
            .map(|h| h.entry_path.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        store
            .memories_for_paths(&paths, cfg.max_items * 4)
            .unwrap_or_default()
    };
    if candidates.len() < cfg.max_items {
        let seen: std::collections::HashSet<i64> = candidates.iter().map(|m| m.id).collect();
        let general = store
            .active_memories(Some(&kinds), cfg.min_confidence, cfg.max_items * 2)
            .unwrap_or_default();
        candidates.extend(general.into_iter().filter(|m| !seen.contains(&m.id)));
    }

    candidates.retain(|m| kinds.contains(&m.kind) && m.confidence >= cfg.min_confidence);
    // Most trustworthy first, so a block truncated by the budget keeps the best of it.
    candidates.sort_by(|a, b| {
        b.kind
            .trust_rank()
            .cmp(&a.kind.trust_rank())
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.verified_at.cmp(&a.verified_at))
            .then(a.id.cmp(&b.id))
    });
    candidates.truncate(cfg.max_items);
    if candidates.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(budget.min(4096));
    out.push_str(HEADER);
    out.push('\n');
    if out.len() > budget {
        // Not even the header fits — emit nothing rather than an unlabelled list of claims,
        // which is strictly worse than no claims at all.
        return String::new();
    }
    let mut wrote_any = false;
    for m in &candidates {
        let line = render_line(m);
        if out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
        wrote_any = true;
    }
    if !wrote_any {
        return String::new();
    }
    out
}

/// One claim, with everything the model needs to discount it: kind, confidence, whether it was
/// ever checked, and where it came from.
fn render_line(m: &MemoryRecord) -> String {
    let mut meta = format!("{} {:.2}", m.kind.as_str(), m.confidence);
    match m.verified_at {
        Some(_) if m.verify_status == indexa_core::memory::VerifyStatus::Verified => {
            meta.push_str(" · verified")
        }
        _ => meta.push_str(" · unverified"),
    }
    if let Some(p) = &m.source_path {
        meta.push_str(" · ");
        meta.push_str(p);
    } else if !m.subject.is_empty() {
        meta.push_str(" · ");
        meta.push_str(&m.subject);
    }
    // Claims are short by construction (a sentence or two); a very long one is still clipped so
    // one rambling memory can't consume the whole block.
    format!("- [{meta}] {}\n", indexa_core::truncate_chars(&m.text, 300))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::memory::{Author, NewMemory, VerifyStatus};

    fn cfg_on() -> MemoryConfig {
        MemoryConfig {
            retrieval: true,
            ..MemoryConfig::default()
        }
    }

    fn store_with(claims: &[(MemoryKind, &str, f32)]) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        for (kind, text, conf) in claims {
            let mut m = NewMemory::new(*kind, *text, Author::Operator);
            m.confidence = *conf;
            s.record_memory(&m).unwrap();
        }
        s
    }

    #[test]
    fn disabled_produces_nothing() {
        let s = store_with(&[(MemoryKind::Observed, "a fact", 0.9)]);
        assert_eq!(
            build_memory_block(&s, &MemoryConfig::default(), &[], 4000),
            ""
        );
    }

    #[test]
    fn an_empty_store_produces_nothing() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(build_memory_block(&s, &cfg_on(), &[], 4000), "");
    }

    #[test]
    fn a_claim_is_rendered_with_its_kind_confidence_and_check_state() {
        let s = store_with(&[(MemoryKind::Observed, "auth uses JWT", 0.9)]);
        let block = build_memory_block(&s, &cfg_on(), &[], 4000);
        assert!(block.starts_with("RECORDED MEMORY"), "got: {block}");
        assert!(block.contains("NOT retrieved file content"), "got: {block}");
        assert!(block.contains("never a memory line"), "got: {block}");
        assert!(
            block.contains("[observed 0.90 · unverified] auth uses JWT"),
            "got: {block}"
        );
    }

    #[test]
    fn inferred_and_hypothesis_are_excluded_by_default() {
        // The default include_kinds is the three evidenced kinds. Feeding a model its own
        // unverified guesses back as context is how a memory system compounds its mistakes.
        let s = store_with(&[
            (MemoryKind::Inferred, "probably a race", 0.9),
            (MemoryKind::Hypothesis, "maybe caching", 0.9),
        ]);
        assert_eq!(build_memory_block(&s, &cfg_on(), &[], 4000), "");
    }

    #[test]
    fn opting_inferred_in_includes_it() {
        let s = store_with(&[(MemoryKind::Inferred, "probably a race", 0.9)]);
        let cfg = MemoryConfig {
            retrieval: true,
            include_kinds: vec!["inferred".to_owned()],
            ..MemoryConfig::default()
        };
        assert!(build_memory_block(&s, &cfg, &[], 4000).contains("probably a race"));
    }

    #[test]
    fn claims_below_the_confidence_floor_are_dropped() {
        let s = store_with(&[
            (MemoryKind::Observed, "confident", 0.9),
            (MemoryKind::Observed, "shaky", 0.1),
        ]);
        let block = build_memory_block(&s, &cfg_on(), &[], 4000);
        assert!(block.contains("confident"));
        assert!(!block.contains("shaky"), "got: {block}");
    }

    #[test]
    fn ordering_puts_the_most_trustworthy_first() {
        let s = store_with(&[
            (MemoryKind::Recalled, "from an old session", 1.0),
            (MemoryKind::Observed, "witnessed", 0.6),
        ]);
        let block = build_memory_block(&s, &cfg_on(), &[], 4000);
        let obs = block.find("witnessed").unwrap();
        let rec = block.find("from an old session").unwrap();
        assert!(
            obs < rec,
            "a witnessed claim outranks a recalled one: {block}"
        );
    }

    #[test]
    fn a_failed_claim_never_reaches_a_prompt() {
        let mut s = store_with(&[(MemoryKind::Observed, "was true once", 0.9)]);
        let id = s.active_memories(None, 0.0, 10).unwrap()[0].id;
        s.set_memory_verify_status(id, VerifyStatus::Failed, Some(1))
            .unwrap();
        assert_eq!(build_memory_block(&s, &cfg_on(), &[], 4000), "");
    }

    #[test]
    fn an_expired_claim_never_reaches_a_prompt() {
        let mut s = store_with(&[(MemoryKind::Observed, "no longer true", 0.9)]);
        let id = s.active_memories(None, 0.0, 10).unwrap()[0].id;
        s.set_memory_valid_to(id, 1).unwrap();
        assert_eq!(build_memory_block(&s, &cfg_on(), &[], 4000), "");
    }

    #[test]
    fn max_items_caps_the_block() {
        let claims: Vec<(MemoryKind, String, f32)> = (0..10)
            .map(|i| (MemoryKind::Observed, format!("claim number {i}"), 0.9))
            .collect();
        let mut s = Store::open_in_memory().unwrap();
        for (k, t, c) in &claims {
            let mut m = NewMemory::new(*k, t.as_str(), Author::Operator);
            m.confidence = *c;
            s.record_memory(&m).unwrap();
        }
        let block = build_memory_block(&s, &cfg_on(), &[], 40_000);
        assert_eq!(block.lines().filter(|l| l.starts_with("- [")).count(), 5);
    }

    #[test]
    fn the_block_never_exceeds_its_byte_budget_and_cuts_at_an_item_boundary() {
        let mut s = Store::open_in_memory().unwrap();
        for i in 0..10 {
            let mut m = NewMemory::new(
                MemoryKind::Observed,
                format!("a reasonably long claim number {i} about the retrieval pipeline"),
                Author::Operator,
            );
            m.confidence = 0.9;
            s.record_memory(&m).unwrap();
        }
        for budget in [200, 400, 600, 800] {
            let block = build_memory_block(&s, &cfg_on(), &[], budget);
            assert!(
                block.len() <= budget,
                "budget {budget} exceeded: {} bytes",
                block.len()
            );
            // Every emitted claim line is whole — a half-claim reads as a different claim.
            for line in block.lines().filter(|l| l.starts_with("- [")) {
                assert!(
                    line.ends_with(|c: char| c.is_alphanumeric() || c == ')' || c == '.'),
                    "truncated mid-claim: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_budget_too_small_for_the_header_emits_nothing() {
        // An unlabelled list of claims is strictly worse than no claims: the model would read
        // somebody's note as though it were retrieved source.
        let s = store_with(&[(MemoryKind::Observed, "a fact", 0.9)]);
        assert_eq!(build_memory_block(&s, &cfg_on(), &[], 20), "");
    }

    #[test]
    fn a_utf8_claim_survives_truncation_intact() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = NewMemory::new(
            MemoryKind::Observed,
            "الفهرس يخزن الملخصات في نفس فضاء المتجهات",
            Author::Operator,
        );
        m.confidence = 0.9;
        s.record_memory(&m).unwrap();
        for budget in [250, 300, 350, 400, 500] {
            let block = build_memory_block(&s, &cfg_on(), &[], budget);
            assert!(block.len() <= budget);
            assert!(std::str::from_utf8(block.as_bytes()).is_ok());
        }
    }
}
