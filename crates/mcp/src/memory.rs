//! Durable-memory tools: `memory_record`, `memory_search`, `memory_update`.
//!
//! # What an agent may and may not do here
//!
//! An agent can **write** claims and **read** them back. It cannot verify one, and it cannot
//! claim more than 0.75 confidence. Both limits are enforced here rather than trusted to the
//! caller:
//!
//! - `author` is forced to [`Author::Agent`], which is what caps confidence. The CLI defaults to
//!   operator; that single difference is the whole ceiling mechanism.
//! - There is deliberately **no** `memory_verify` tool. An agent cannot independently confirm
//!   its own claim — asking it to would just produce a second assertion from the same source.
//!   Verification is operator authority: `indexa memory verify` from the CLI, where a human
//!   either sees the source hash compared or explicitly runs the stored command.
//!
//! `verify_cmd` is stored as data and never executed by this crate.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::memory::{Author, MemoryKind, MemoryRecord, NewMemory};

use crate::{mcp_err, mcp_invalid, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRecordParams {
    /// The claim itself, in plain language. One or two sentences.
    pub text: String,
    /// How the claim is known — pick honestly, it determines how far it is trusted later:
    /// `observed` (you saw it in code, a test run, a file or a tool's output),
    /// `stated` (the user or a project doc told you), `inferred` (you reasoned it out),
    /// `recalled` (carried forward from an earlier session), `hypothesis` (a guess).
    pub kind: String,
    /// What the claim is about: a file path, a symbol, or a free-form topic key.
    #[serde(default)]
    pub subject: Option<String>,
    /// Confidence 0.0-1.0. Capped at 0.75 for an agent-authored claim; the response says so
    /// when it clamps.
    #[serde(default)]
    pub confidence: Option<f32>,
    /// File this claim was drawn from. Its content is hashed now, so a later `indexa memory
    /// verify` can tell whether the source moved on underneath the claim.
    #[serde(default)]
    pub source_path: Option<String>,
    /// Command a human could run to re-check this claim. Stored and shown, never executed.
    #[serde(default)]
    pub verify_cmd: Option<String>,
    /// Optional tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Other paths this claim is relevant to.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemorySearchParams {
    /// Words to look for across claim text and tags. Omit to list by trust instead.
    #[serde(default)]
    pub query: Option<String>,
    /// Only claims about these paths (matched on subject or associated paths).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only these kinds.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Minimum confidence (default 0.0).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Maximum results (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryUpdateParams {
    /// The memory to act on.
    pub id: i64,
    /// `supersede` (replace it with a corrected claim, keeping both and the link between them)
    /// or `retire` (stop offering it, keep the record).
    pub action: String,
    /// The corrected claim. Required for `supersede`.
    #[serde(default)]
    pub text: Option<String>,
}

fn parse_kind(s: &str) -> Result<MemoryKind, ErrorData> {
    MemoryKind::parse(s).ok_or_else(|| {
        let all = MemoryKind::ALL
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        mcp_invalid(format!("unknown kind '{s}' — expected one of: {all}"))
    })
}

/// One claim, rendered for an agent to read: enough metadata to discount it, never so much
/// that the claim itself gets lost.
fn render(m: &MemoryRecord) -> String {
    let mut s = format!(
        "#{} [{} {:.2} · {}]",
        m.id,
        m.kind.as_str(),
        m.confidence,
        m.verify_status.as_str()
    );
    if !m.subject.is_empty() {
        s.push_str(&format!(" · {}", m.subject));
    }
    s.push_str(&format!("\n  {}", m.text));
    if let Some(c) = &m.verify_cmd {
        s.push_str(&format!("\n  (re-check with: {c})"));
    }
    s
}

#[tool_router(router = router_memory, vis = "pub(crate)")]
impl IndexaMcp {
    #[tool(
        description = "Record a durable claim you have learned about this project — a fact, a \
constraint, a correction, or a lead — so it survives this session. Pick `kind` honestly: \
`observed` (witnessed in code, a test run, a file or tool output), `stated` (the user or a doc \
told you), `inferred` (you reasoned it out), `recalled` (from an earlier session), `hypothesis` \
(a guess). Mislabelling an inference as an observation is how a memory store starts repeating \
your guesses back as facts. Your claims are capped at 0.75 confidence; only the user can raise \
one. Prefer this over add_note for anything you want retrievable immediately with its \
provenance attached.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub(crate) async fn memory_record(
        &self,
        params: Parameters<MemoryRecordParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        if p.text.trim().is_empty() {
            return Err(mcp_invalid("a memory needs a non-empty claim"));
        }
        let kind = parse_kind(&p.kind)?;
        // Forced, not defaulted: an agent must not be able to author a claim as the operator
        // and thereby escape the confidence ceiling.
        let mut m = NewMemory::new(kind, p.text, Author::Agent);
        m.subject = p.subject.unwrap_or_default();
        if let Some(c) = p.confidence {
            m.confidence = c;
        }
        m.verify_cmd = p.verify_cmd;
        m.tags = p.tags;
        m.paths = p.paths;
        if let Some(src) = p.source_path {
            m.source_sha256 = indexa_core::memory::source_hash(std::path::Path::new(&src));
            m.source_path = Some(src);
        }

        let mut store = self.store()?;
        let (id, clamped) = store.record_memory(&m).map_err(mcp_err)?;
        let stored = store
            .memory_by_id(id)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(anyhow::anyhow!("memory #{id} vanished after write")))?;

        let mut out = format!("Recorded memory #{id}.\n{}", render(&stored));
        if clamped {
            out.push_str(&format!(
                "\n\nConfidence recorded as {:.2} — agent-authored claims are capped at {:.2}. \
                 The user can raise it with `indexa memory verify {id}`.",
                stored.confidence,
                Author::Agent.max_confidence()
            ));
        }
        Ok(ok_text(out))
    }

    #[tool(
        description = "Look up durable claims recorded about this project — what you or the user \
concluded in earlier sessions, with each claim's kind, confidence and whether it was ever \
verified. Use it before re-deriving something you may already have worked out, and to check \
whether a constraint was recorded for a file you are about to change. Pass `paths` to get \
claims about specific files, `query` to search the text, or neither to list the most trusted.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn memory_search(
        &self,
        params: Parameters<MemorySearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let limit = p.limit.unwrap_or(20).clamp(1, 200);
        let min_conf = p.min_confidence.unwrap_or(0.0);
        let kinds: Vec<MemoryKind> = p
            .kinds
            .iter()
            .map(|k| parse_kind(k))
            .collect::<Result<Vec<_>, _>>()?;
        let query = p.query.as_deref().filter(|q| !q.trim().is_empty());
        let store = self.store()?;

        // `query`, `paths`, `kinds` and `min_confidence` are filters meant to compose, not
        // alternate modes — an agent asking for `paths=[…], kinds=[…]` expects every clause to
        // apply, not for `paths` to silently win and `query`/`kinds`/`min_confidence` to be
        // ignored or applied too late to matter. Both store calls below apply every requested
        // filter in SQL — inside the `WHERE`, ahead of `ORDER BY`/`LIMIT` — so the caller's
        // `limit` is the last thing evaluated, never a candidate cap that runs before `kinds` or
        // `min_confidence` can narrow the set. A cap-then-filter composition (this endpoint's
        // previous shape) can silently drop a match that exists but isn't among the first rows a
        // capped fetch happens to return; see `Store::memories_matching`'s doc comment.
        let kind_filter = (!kinds.is_empty()).then_some(kinds.as_slice());
        let rows = if !p.paths.is_empty() || query.is_some() {
            store
                .memories_matching(query, &p.paths, kind_filter, min_conf, limit)
                .map_err(mcp_err)?
        } else {
            store
                .active_memories(kind_filter, min_conf, limit)
                .map_err(mcp_err)?
        };

        if rows.is_empty() {
            return Ok(ok_text(
                "No matching memories. Nothing has been recorded about this yet — \
                 `memory_record` anything you work out that would be worth having next time.",
            ));
        }
        let body = rows.iter().map(render).collect::<Vec<_>>().join("\n\n");
        Ok(ok_text(format!(
            "{} memory/memories (most trusted first):\n\n{body}\n\nThese are recorded CLAIMS, \
             not retrieved file content — check anything marked `unverified` or \
             `hypothesis` before relying on it.",
            rows.len()
        )))
    }

    #[tool(
        description = "Correct or withdraw a recorded claim. `supersede` replaces it with a \
corrected claim, keeping the original and the link between them so the history of a changing \
belief stays readable; `retire` stops it being offered without deleting it. Use supersede when \
you learn a recorded claim was wrong — leaving a stale claim in place is worse than never \
having recorded it. Verification is not available to you: only the user can mark a claim \
verified.",
        // Destructive, per the annotation policy in lib.rs's
        // `tools_carry_read_only_or_destructive_annotations`: is there another exposed tool that
        // undoes this one's effect? `supersede` keeps the original row and links it, but `retire`
        // has no MCP-exposed undo — there is no `unretire` tool, and re-recording the same text
        // creates a fresh agent/unverified claim rather than restoring the retired row's id,
        // verification status and provenance. Neither row is ever deleted, but that is not the
        // bar this policy uses; a tool that can put a claim into a state nothing else here can
        // reverse is destructive even though it keeps every row, same as `dismiss_decision` and
        // `ignore_classification`.
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    pub(crate) async fn memory_update(
        &self,
        params: Parameters<MemoryUpdateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let mut store = self.store()?;
        let Some(existing) = store.memory_by_id(p.id).map_err(mcp_err)? else {
            return Err(mcp_invalid(format!("no memory #{}", p.id)));
        };
        match p.action.trim().to_ascii_lowercase().as_str() {
            "retire" => {
                store.retire_memory(p.id).map_err(mcp_err)?;
                Ok(ok_text(format!(
                    "Memory #{} retired. The row is kept; it will not be offered as context.",
                    p.id
                )))
            }
            "supersede" => {
                let text = p
                    .text
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| mcp_invalid("supersede needs `text`: the corrected claim"))?;
                let mut m = NewMemory::new(existing.kind, text, Author::Agent);
                m.subject = existing.subject.clone();
                m.confidence = existing.confidence;
                m.source_path = existing.source_path.clone();
                m.source_sha256 = existing
                    .source_path
                    .as_ref()
                    .and_then(|p| indexa_core::memory::source_hash(std::path::Path::new(p)));
                m.tags = existing.tags.clone();
                m.paths = existing.paths.clone();
                let (new_id, _) = store.supersede_memory(p.id, &m).map_err(mcp_err)?;
                if new_id == p.id {
                    return Ok(ok_text(format!(
                        "Unchanged — the replacement text is identical to memory #{}.",
                        p.id
                    )));
                }
                Ok(ok_text(format!(
                    "Memory #{} replaced by #{new_id}. The original is retired, not deleted.",
                    p.id
                )))
            }
            other => Err(mcp_invalid(format!(
                "unknown action '{other}' — expected `supersede` or `retire`. \
                 Verification is operator-only (`indexa memory verify`)."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::store::Store;

    // ── MCP-handler tests (real IndexaMcp against a temp on-disk index) ──

    struct StubEmbedder;
    #[async_trait::async_trait]
    impl indexa_embed::Embedder for StubEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }
    struct StubGenerator;
    #[async_trait::async_trait]
    impl indexa_llm::Generator for StubGenerator {
        async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok("stub".to_owned())
        }
    }

    /// An `IndexaMcp` over a fresh temp-file index, seeded by `seed` before the handle is
    /// constructed (mirrors `lib.rs`'s `mcp_with_db`, plus the seed hook these tests need).
    fn mcp_with_db(dbdir: &tempfile::TempDir, seed: impl FnOnce(&mut Store)) -> IndexaMcp {
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            seed(&mut store);
        }
        IndexaMcp::new(
            dbpath,
            std::sync::Arc::new(StubEmbedder),
            std::sync::Arc::new(StubGenerator),
            std::sync::Arc::new(indexa_core::config::Config::default()),
        )
    }

    /// Concatenate a `CallToolResult`'s text content blocks (mirrors `lib.rs`'s `tool_text`).
    fn text_of(r: CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The exact fixture from the #538 review: an observed claim at higher confidence and an
    /// inferred claim at lower confidence, both about `/x`. `paths=[/x], kinds=[inferred],
    /// limit=1` must return the inferred claim. The previous composition fetched only `limit`
    /// (1) path-matched row — ordered by confidence, so the observed claim filled that one
    /// slot — then filtered by `kinds` afterward, dropping it and reporting "No matching
    /// memories" despite the inferred claim genuinely existing.
    #[tokio::test]
    async fn memory_search_composes_paths_kinds_and_limit_instead_of_dropping_a_match() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir, |store| {
            let mut observed = NewMemory::new(
                MemoryKind::Observed,
                "/x uses LRU eviction",
                Author::Operator,
            );
            observed.subject = "/x".into();
            observed.confidence = 0.75;
            store.record_memory(&observed).unwrap();

            let mut inferred = NewMemory::new(
                MemoryKind::Inferred,
                "/x might leak file handles",
                Author::Operator,
            );
            inferred.subject = "/x".into();
            inferred.confidence = 0.50;
            store.record_memory(&inferred).unwrap();
        });

        let out = text_of(
            mcp.memory_search(Parameters(MemorySearchParams {
                query: None,
                paths: vec!["/x".into()],
                kinds: vec!["inferred".into()],
                min_confidence: None,
                limit: Some(1),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("might leak"),
            "the inferred claim about /x must be returned, got: {out}"
        );
        assert!(
            !out.contains("No matching memories"),
            "a real match exists and must not be reported as none, got: {out}"
        );
    }

    /// `paths` and `query` are filters meant to compose (an AND), not alternate modes where
    /// `paths` silently wins. Two claims are both about `/x`; only one mentions "LRU". Asking
    /// for `paths=[/x]` AND `query="LRU"` must return only the matching one.
    #[tokio::test]
    async fn memory_search_intersects_query_and_paths_instead_of_ignoring_the_query() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir, |store| {
            let mut cache = NewMemory::new(
                MemoryKind::Observed,
                "the /x cache uses LRU eviction",
                Author::Operator,
            );
            cache.subject = "/x".into();
            store.record_memory(&cache).unwrap();

            let mut unrelated = NewMemory::new(
                MemoryKind::Observed,
                "the /x handler retries on timeout",
                Author::Operator,
            );
            unrelated.subject = "/x".into();
            store.record_memory(&unrelated).unwrap();
        });

        let out = text_of(
            mcp.memory_search(Parameters(MemorySearchParams {
                query: Some("LRU".into()),
                paths: vec!["/x".into()],
                kinds: vec![],
                min_confidence: None,
                limit: Some(20),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("LRU eviction"),
            "the LRU claim about /x must be returned, got: {out}"
        );
        assert!(
            !out.contains("retries on timeout"),
            "the non-matching /x claim must be excluded once `query` narrows it, got: {out}"
        );
    }

    /// The re-review's "two-row manifestation": with neither `paths` nor `query` given,
    /// `memory_search` falls through to `active_memories`, whose SQL used to apply `LIMIT`
    /// before the `kinds` filter that used to run afterward in Rust. Two claims, one observed
    /// at higher confidence and one inferred at lower confidence; asking for `kinds=[inferred],
    /// limit=1` must return the inferred claim rather than let the higher-confidence,
    /// wrong-kind claim fill the only slot and get filtered away with nothing left.
    #[tokio::test]
    async fn memory_search_kinds_filter_survives_a_tight_limit_with_no_paths_or_query() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir, |store| {
            let mut observed =
                NewMemory::new(MemoryKind::Observed, "seen directly", Author::Operator);
            observed.confidence = 0.75;
            store.record_memory(&observed).unwrap();

            let mut inferred =
                NewMemory::new(MemoryKind::Inferred, "reasoned out", Author::Operator);
            inferred.confidence = 0.50;
            store.record_memory(&inferred).unwrap();
        });

        let out = text_of(
            mcp.memory_search(Parameters(MemorySearchParams {
                query: None,
                paths: vec![],
                kinds: vec!["inferred".into()],
                min_confidence: None,
                limit: Some(1),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("reasoned out"),
            "the inferred claim must be returned even though a higher-confidence observed \
             claim would otherwise fill the only slot, got: {out}"
        );
        assert!(
            !out.contains("No matching memories"),
            "a real match exists, got: {out}"
        );
    }

    /// `min_confidence` and `kinds` must compose together, not just each alone: a wrong-kind
    /// claim ranks first by confidence, one right-kind claim clears `min_confidence`, and
    /// another right-kind claim doesn't. `limit=1` must return the one claim satisfying every
    /// filter, not the wrong-kind claim that would fill the slot if `kinds` were applied after
    /// `LIMIT`.
    #[tokio::test]
    async fn memory_search_composes_min_confidence_with_kinds_and_a_tight_limit() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir, |store| {
            let mut wrong_kind =
                NewMemory::new(MemoryKind::Stated, "the user said so", Author::Operator);
            wrong_kind.confidence = 0.9;
            store.record_memory(&wrong_kind).unwrap();

            let mut too_low =
                NewMemory::new(MemoryKind::Inferred, "a shaky guess", Author::Operator);
            too_low.confidence = 0.3;
            store.record_memory(&too_low).unwrap();

            let mut wanted =
                NewMemory::new(MemoryKind::Inferred, "a solid inference", Author::Operator);
            wanted.confidence = 0.6;
            store.record_memory(&wanted).unwrap();
        });

        let out = text_of(
            mcp.memory_search(Parameters(MemorySearchParams {
                query: None,
                paths: vec![],
                kinds: vec!["inferred".into()],
                min_confidence: Some(0.5),
                limit: Some(1),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("a solid inference"),
            "the one claim meeting both the kind and confidence filters must be returned, \
             got: {out}"
        );
        assert!(!out.contains("the user said so"), "got: {out}");
        assert!(!out.contains("a shaky guess"), "got: {out}");
    }

    /// The exact "single remaining finding" from the #538 re-review: a narrowing store lookup
    /// (`paths` here) used to run its own `SELECT … LIMIT` ahead of `kinds`/`min_confidence`,
    /// capped generously (10,000) but still a cap — so a store holding more matching rows than
    /// that cap, none of them individually unrealistic, can push a real match past the cutoff
    /// before `kinds` ever gets to filter on it. 10,000 observed claims about `/x` at 0.75
    /// confidence, plus one inferred claim about `/x` at 0.50, reproduces the re-review's
    /// fixture at the scale where the old cap actually mattered.
    #[tokio::test]
    async fn memory_search_does_not_drop_a_match_behind_a_large_prefiltered_candidate_set() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir, |store| {
            for i in 0..10_000 {
                let mut filler = NewMemory::new(
                    MemoryKind::Observed,
                    format!("/x filler claim #{i}"),
                    Author::Operator,
                );
                filler.subject = "/x".into();
                filler.confidence = 0.75;
                store.record_memory(&filler).unwrap();
            }
            let mut inferred = NewMemory::new(
                MemoryKind::Inferred,
                "/x might leak file handles",
                Author::Operator,
            );
            inferred.subject = "/x".into();
            inferred.confidence = 0.50;
            store.record_memory(&inferred).unwrap();
        });

        let out = text_of(
            mcp.memory_search(Parameters(MemorySearchParams {
                query: None,
                paths: vec!["/x".into()],
                kinds: vec!["inferred".into()],
                min_confidence: None,
                limit: Some(1),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("might leak"),
            "the inferred claim about /x must be returned even with 10,000 higher-confidence \
             observed claims about /x ahead of it, got: {out}"
        );
    }

    /// No candidate satisfies the requested filters: the response must say so plainly rather
    /// than erroring or returning an unrelated claim.
    #[tokio::test]
    async fn memory_search_reports_no_match_when_nothing_qualifies() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir, |store| {
            let mut observed = NewMemory::new(
                MemoryKind::Observed,
                "/x uses LRU eviction",
                Author::Operator,
            );
            observed.subject = "/x".into();
            observed.confidence = 0.75;
            store.record_memory(&observed).unwrap();
        });

        let out = text_of(
            mcp.memory_search(Parameters(MemorySearchParams {
                query: None,
                paths: vec!["/x".into()],
                kinds: vec!["hypothesis".into()],
                min_confidence: None,
                limit: Some(20),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("No matching memories"),
            "no hypothesis claim about /x exists, got: {out}"
        );
    }

    /// The confidence ceiling exists because the agent writing a claim is usually the one that
    /// later reads its own number back as though it were independent evidence. It must be
    /// enforced by the server, not requested of the caller.
    #[test]
    fn an_agent_authored_claim_is_capped_regardless_of_what_was_asked_for() {
        let mut store = Store::open_in_memory().unwrap();
        let mut m = NewMemory::new(MemoryKind::Inferred, "probably a race", Author::Agent);
        m.confidence = 1.0;
        let (id, clamped) = store.record_memory(&m).unwrap();
        assert!(clamped);
        assert!((store.memory_by_id(id).unwrap().unwrap().confidence - 0.75).abs() < 1e-6);
    }

    #[test]
    fn unknown_kinds_are_rejected_with_the_valid_set_named() {
        let err = parse_kind("pratyaksha").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("observed"),
            "the error must list valid kinds: {msg}"
        );
    }

    #[test]
    fn every_documented_kind_parses() {
        for k in MemoryKind::ALL {
            assert!(parse_kind(k.as_str()).is_ok(), "{k}");
        }
    }

    #[test]
    fn render_shows_what_is_needed_to_discount_a_claim() {
        let mut store = Store::open_in_memory().unwrap();
        let mut m = NewMemory::new(MemoryKind::Hypothesis, "maybe caching", Author::Agent);
        m.subject = "src/cache.rs".into();
        m.verify_cmd = Some("cargo test cache".into());
        let (id, _) = store.record_memory(&m).unwrap();
        let out = render(&store.memory_by_id(id).unwrap().unwrap());
        assert!(out.contains("hypothesis"), "{out}");
        assert!(out.contains("unverified"), "{out}");
        assert!(out.contains("src/cache.rs"), "{out}");
        assert!(out.contains("re-check with: cargo test cache"), "{out}");
    }

    /// An agent must not be able to promote its own claim by asserting verification.
    #[test]
    fn there_is_no_agent_facing_verify_action() {
        let src = include_str!("memory.rs");
        // The needles are split so they do not appear as literals in this file — it
        // `include_str!`s itself, so a plain literal would match its own assertion and the
        // guard would be permanently, silently satisfied.
        assert!(
            !src.contains(concat!("set_memory_", "verify_status")),
            "the MCP surface must never set a verification status — verification is \
             operator authority, and an agent confirming its own claim is not verification"
        );
        // Match a tool FUNCTION, not the word — the module docs deliberately explain why no
        // such tool exists, and a bare-word check would flag its own explanation.
        assert!(
            !src.contains(concat!("fn memory_", "verify")),
            "there must be no agent-facing verify tool"
        );
    }
}
