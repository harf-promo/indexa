//! Typed durable memory: claims Indexa is told, or works out, and keeps.
//!
//! # Why this is not the Decision Ledger
//!
//! [`crate::decisions`] answers "a question Indexa raised and the user answered". Its schema is
//! structurally a Q&A: a partial unique index allows **one open row per subject**, `effects` /
//! `effects_applied_at` form a crash-safe *projection* contract onto `classifications` and
//! `importance_weights`, `gc_decisions` prunes resolved rows, and `options` / `auto_value` /
//! `chosen` / `priority` are all "which candidate did the user pick".
//!
//! A memory has none of that. It is many-per-subject, it *is* the durable state rather than
//! projecting onto something else, it must never be garbage-collected once an operator has
//! verified it, and it has no candidate list. Storing memories in `decisions` would mean five
//! permanently-NULL columns per row, permanent `unapplied_decided()` repair-sweep targets, and
//! — worst — an agent that has learned "memory lives in the ledger" starting to *answer*
//! memories as if they were open questions, since `list_open_decisions` is a core MCP tool.
//!
//! What is reused rather than reinvented: `decisions::patch_id` for git-anchoring, and the
//! revision-chain shape (`parent_id` / `superseded_by`), which is six lines of DDL and worth
//! owning separately so memory supersession is not coupled to ledger supersession.
//!
//! # The five kinds
//!
//! The taxonomy exists so that "the test suite passes on aarch64" and "I think this is why the
//! test is flaky" cannot be retrieved as though they were the same sort of claim. Collapsing
//! them into undifferentiated "memory" is what makes a memory system start confidently
//! repeating its own guesses back to you.

use std::fmt;

/// What kind of claim a memory is — and therefore how far to trust it.
///
/// Ordered from most to least directly evidenced. [`MemoryKind::trust_rank`] is that order made
/// numeric; it drives retrieval ordering and decay eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemoryKind {
    /// Directly observed from code, a test run, a file, or a tool's output. The strongest kind:
    /// something actually happened and was witnessed.
    Observed,
    /// Asserted by a trusted source — the operator, a project doc, an explicit instruction.
    /// Not independently checked, but authoritative because of who said it.
    Stated,
    /// Derived by reasoning from other claims. Plausible, unwitnessed, and the first thing that
    /// should age when nothing has confirmed it.
    Inferred,
    /// Carried forward from an earlier session or an ingested transcript. Preserved so context
    /// survives compaction; explicitly NOT a fresh observation, even if it reads like one.
    Recalled,
    /// A guess, a lead, a possibility worth writing down. Never to be presented as fact.
    Hypothesis,
}

impl MemoryKind {
    /// Every kind, most-trusted first. The order is the contract — [`Self::trust_rank`] is
    /// derived from it, so a new kind is placed by editing this array alone.
    pub const ALL: [MemoryKind; 5] = [
        MemoryKind::Observed,
        MemoryKind::Stated,
        MemoryKind::Inferred,
        MemoryKind::Recalled,
        MemoryKind::Hypothesis,
    ];

    /// The stored (and user-facing) spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Observed => "observed",
            MemoryKind::Stated => "stated",
            MemoryKind::Inferred => "inferred",
            MemoryKind::Recalled => "recalled",
            MemoryKind::Hypothesis => "hypothesis",
        }
    }

    /// Parse the stored spelling. Case-insensitive so a hand-written config or a CLI argument
    /// doesn't fail on capitalization.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// How far to trust this kind, higher is better. Used to order the retrieval block so the
    /// model reads witnessed facts before guesses, and to decide what may be aged.
    pub fn trust_rank(self) -> u8 {
        // Derived from ALL's ordering so the two can never disagree.
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        (Self::ALL.len() - idx) as u8
    }

    /// May an *unverified* memory of this kind be aged by `indexa memory decay`?
    ///
    /// Only inference and hypothesis. An observation, a trusted statement, or a recalled fact
    /// does not become less true because time passed — if it stopped being true, that is a
    /// supersession or a failed verification, not decay. Deliberately conservative: the failure
    /// mode of aging too eagerly is a memory system that quietly forgets what it was told.
    pub fn decayable(self) -> bool {
        matches!(self, MemoryKind::Inferred | MemoryKind::Hypothesis)
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who authored a memory. Determines the confidence ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Author {
    /// Written by an AI agent through the MCP surface.
    Agent,
    /// Written by a human through the CLI or the web UI.
    Operator,
}

impl Author {
    pub fn as_str(self) -> &'static str {
        match self {
            Author::Agent => "agent",
            Author::Operator => "operator",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agent" => Some(Author::Agent),
            "operator" | "user" => Some(Author::Operator),
            _ => None,
        }
    }

    /// The highest confidence this author may claim.
    ///
    /// An agent is capped at 0.75 because an agent asserting certainty about its own inference
    /// is precisely the failure this whole module exists to contain — and because the agent
    /// writing the claim is usually the same one that will later read it back and treat its own
    /// number as independent evidence. Only a human, or a passing verification, lifts a claim
    /// above that ceiling.
    pub fn max_confidence(self) -> f32 {
        match self {
            Author::Agent => 0.75,
            Author::Operator => 1.0,
        }
    }
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a memory's claim has been checked, and how that went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyStatus {
    /// Never checked. The default, and the only status decay considers.
    Unverified,
    /// Checked and held.
    Verified,
    /// Checked and did NOT hold — the source changed, or the verify command failed. Excluded
    /// from retrieval, kept for the record rather than deleted.
    Failed,
    /// Aged by `indexa memory decay`: unverified, past its window, confidence lowered. Still
    /// readable, still never deleted.
    Aged,
}

impl VerifyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyStatus::Unverified => "unverified",
            VerifyStatus::Verified => "verified",
            VerifyStatus::Failed => "failed",
            VerifyStatus::Aged => "aged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "unverified" => Some(VerifyStatus::Unverified),
            "verified" => Some(VerifyStatus::Verified),
            "failed" => Some(VerifyStatus::Failed),
            "aged" => Some(VerifyStatus::Aged),
            _ => None,
        }
    }

    /// Should a memory in this state be offered to retrieval? A failed claim is known-wrong;
    /// everything else is fair game with its confidence attached.
    pub fn retrievable(self) -> bool {
        !matches!(self, VerifyStatus::Failed)
    }
}

impl fmt::Display for VerifyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state, orthogonal to verification. `active` is the working set; `retired` is kept
/// for provenance but never retrieved or superseded further.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryStatus {
    Active,
    Retired,
}

impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryStatus::Active => "active",
            MemoryStatus::Retired => "retired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Some(MemoryStatus::Active),
            "retired" => Some(MemoryStatus::Retired),
            _ => None,
        }
    }
}

impl fmt::Display for MemoryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SHA-256 of a source file's current content, as recorded on a memory's `source_sha256`.
///
/// `None` when the file cannot be read — which is **not** an error condition: a claim about a
/// file that has since been deleted is often the most valuable thing in the store, so the
/// caller records the claim without a hash rather than refusing it.
///
/// Lives here because three surfaces need exactly this — `memory add`, `memory supersede`, and
/// the MCP `memory_record` tool — and an inlined copy in each is how the two `watch`
/// implementations drifted apart.
pub fn source_hash(path: &std::path::Path) -> Option<String> {
    use sha2::Digest as _;
    std::fs::read(path)
        .ok()
        .map(|b| crate::store::hex_digest(sha2::Sha256::digest(&b)))
}

/// A memory as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecord {
    pub id: i64,
    pub kind: MemoryKind,
    pub text: String,
    /// SHA-256 of `text`, the dedup key among active rows.
    pub text_sha256: String,
    /// What the claim is *about*: a path, a symbol, or a free-form topic key. Empty when the
    /// claim is project-wide.
    pub subject: String,
    pub confidence: f32,
    pub author: Author,
    /// The file this claim was drawn from, when there is one.
    pub source_path: Option<String>,
    /// SHA-256 of `source_path`'s content when the claim was made — what a later `verify`
    /// compares against to decide the claim has gone stale.
    pub source_sha256: Option<String>,
    /// `git patch-id` anchor, so a claim about a change survives rebase and squash.
    pub patch_id: Option<String>,
    /// Set when this row was adopted from a Decision Ledger annotation.
    pub source_decision_id: Option<i64>,
    /// How to re-check the claim. **Stored and printed, never executed automatically** — see
    /// the module docs on `store::memories`.
    pub verify_cmd: Option<String>,
    pub verify_status: VerifyStatus,
    pub verified_at: Option<i64>,
    pub tags: Vec<String>,
    pub status: MemoryStatus,
    pub parent_id: Option<i64>,
    pub superseded_by: Option<i64>,
    /// Bitemporal validity: when the claim started being true, and when it stopped. `valid_to`
    /// is `None` while the claim is still believed. Distinct from `created_at`/`updated_at`,
    /// which record when Indexa was *told*, not when the world changed.
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Paths this memory is relevant to, beyond `subject` — the join used to surface memories
    /// for the files a question is about.
    pub paths: Vec<String>,
}

impl MemoryRecord {
    /// Is this memory eligible to be shown to a model right now?
    pub fn is_live(&self, now: i64) -> bool {
        self.status == MemoryStatus::Active
            && self.verify_status.retrievable()
            && self.valid_to.is_none_or(|t| t > now)
    }
}

/// A memory to record. `confidence` is clamped to the author's ceiling on write.
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub kind: MemoryKind,
    pub text: String,
    pub subject: String,
    pub confidence: f32,
    pub author: Author,
    pub source_path: Option<String>,
    pub source_sha256: Option<String>,
    pub patch_id: Option<String>,
    pub source_decision_id: Option<i64>,
    pub verify_cmd: Option<String>,
    pub tags: Vec<String>,
    pub valid_from: Option<i64>,
    pub paths: Vec<String>,
}

impl NewMemory {
    /// The minimum: a kind and the claim itself. Everything else defaults to "unknown", which
    /// is honest — a memory with no source is still worth keeping, it just can't be verified.
    pub fn new(kind: MemoryKind, text: impl Into<String>, author: Author) -> Self {
        Self {
            kind,
            text: text.into(),
            subject: String::new(),
            // Deliberately mid-scale, not high: an unqualified claim is a starting point.
            confidence: 0.5,
            author,
            source_path: None,
            source_sha256: None,
            patch_id: None,
            source_decision_id: None,
            verify_cmd: None,
            tags: Vec::new(),
            valid_from: None,
            paths: Vec::new(),
        }
    }

    /// The confidence this memory will actually be stored with: the requested value, clamped to
    /// `[0.0, author.max_confidence()]`. Returns `(value, was_clamped)` so a surface can tell
    /// the caller its number was reduced rather than silently lowering it.
    pub fn effective_confidence(&self) -> (f32, bool) {
        let ceiling = self.author.max_confidence();
        let clamped = self.confidence.clamp(0.0, ceiling);
        (clamped, clamped < self.confidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_its_stored_spelling() {
        for k in MemoryKind::ALL {
            assert_eq!(MemoryKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(MemoryKind::parse("  OBSERVED "), Some(MemoryKind::Observed));
        assert_eq!(MemoryKind::parse("pratyaksha"), None);
    }

    #[test]
    fn trust_rank_follows_the_declared_order() {
        // Observed is the most trusted, Hypothesis the least, and the ranks are strictly
        // descending — the retrieval block's ordering depends on that.
        let ranks: Vec<u8> = MemoryKind::ALL.iter().map(|k| k.trust_rank()).collect();
        assert!(
            ranks.windows(2).all(|w| w[0] > w[1]),
            "ranks must strictly descend in ALL order, got {ranks:?}"
        );
        assert!(MemoryKind::Observed.trust_rank() > MemoryKind::Hypothesis.trust_rank());
    }

    #[test]
    fn only_inference_and_hypothesis_may_be_aged() {
        // The decay contract, asserted over every kind rather than the two it allows — so
        // adding a sixth kind forces a decision here instead of silently defaulting to
        // "ageable".
        for k in MemoryKind::ALL {
            let expected = matches!(k, MemoryKind::Inferred | MemoryKind::Hypothesis);
            assert_eq!(k.decayable(), expected, "{k} decayable()");
        }
    }

    #[test]
    fn an_agent_cannot_claim_more_than_three_quarters_confidence() {
        let mut m = NewMemory::new(MemoryKind::Inferred, "the flake is a race", Author::Agent);
        m.confidence = 1.0;
        assert_eq!(m.effective_confidence(), (0.75, true));

        m.confidence = 0.5;
        assert_eq!(m.effective_confidence(), (0.5, false), "under the ceiling");
    }

    #[test]
    fn an_operator_may_claim_certainty() {
        let mut m = NewMemory::new(MemoryKind::Stated, "we ship on Fridays", Author::Operator);
        m.confidence = 1.0;
        assert_eq!(m.effective_confidence(), (1.0, false));
    }

    #[test]
    fn negative_confidence_is_clamped_without_being_reported_as_a_ceiling_hit() {
        let mut m = NewMemory::new(MemoryKind::Observed, "x", Author::Operator);
        m.confidence = -3.0;
        let (v, clamped) = m.effective_confidence();
        assert_eq!(v, 0.0);
        assert!(!clamped, "clamping UP to 0 is not the ceiling being hit");
    }

    #[test]
    fn a_failed_claim_is_not_retrievable_but_an_aged_one_still_is() {
        assert!(!VerifyStatus::Failed.retrievable());
        for s in [
            VerifyStatus::Unverified,
            VerifyStatus::Verified,
            VerifyStatus::Aged,
        ] {
            assert!(s.retrievable(), "{s} should stay retrievable");
        }
    }

    #[test]
    fn liveness_accounts_for_status_verification_and_validity_window() {
        let base = MemoryRecord {
            id: 1,
            kind: MemoryKind::Observed,
            text: "x".into(),
            text_sha256: "h".into(),
            subject: String::new(),
            confidence: 0.9,
            author: Author::Operator,
            source_path: None,
            source_sha256: None,
            patch_id: None,
            source_decision_id: None,
            verify_cmd: None,
            verify_status: VerifyStatus::Verified,
            verified_at: None,
            tags: vec![],
            status: MemoryStatus::Active,
            parent_id: None,
            superseded_by: None,
            valid_from: 0,
            valid_to: None,
            created_at: 0,
            updated_at: 0,
            paths: vec![],
        };
        assert!(base.is_live(100));

        let retired = MemoryRecord {
            status: MemoryStatus::Retired,
            ..base.clone()
        };
        assert!(!retired.is_live(100));

        let failed = MemoryRecord {
            verify_status: VerifyStatus::Failed,
            ..base.clone()
        };
        assert!(!failed.is_live(100));

        // A claim whose validity window has closed is history, not context.
        let expired = MemoryRecord {
            valid_to: Some(50),
            ..base.clone()
        };
        assert!(!expired.is_live(100));
        assert!(expired.is_live(10), "still live before valid_to");
    }
}
