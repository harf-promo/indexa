//! Per-answer "impact": how much smaller the served context was than the cited source files —
//! the concrete, per-query proof of Indexa's "retrieve the slice, don't pack the repo" pitch.

use indexa_core::store::Store;
use indexa_core::text::{human_bytes, human_count};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::qa::Answer;

/// Canonical `tool_usage.served_basis` tags — what a row's `bytes_served` actually measured.
/// The savings ledger blends surfaces that count differently, so each recording site tags its
/// row with one of these so `status`/`get_stats` can reconcile per-surface (see
/// [`Store::usage_by_basis`](indexa_core::store::Store::usage_by_basis)). Kept as `&str`
/// constants (not an enum) because the value crosses into the core store layer as plain text.
///
/// The full rendered tool response (MCP `search`/`get_summary`/`get_chunk_context`/`ask`/`read_file`).
pub const BASIS_RENDERED_RESPONSE: &str = "rendered_response";
/// The answer text plus its delivered citations (web + CLI `ask`; see [`served_bytes`]).
pub const BASIS_ANSWER_CITATIONS: &str = "answer_citations";
/// The answer text only, without citation lines (MCP `ask_catalog`).
pub const BASIS_ANSWER_TEXT: &str = "answer_text";

/// The size of what an [`Answer`] actually delivered to the AI tool, versus the full size of
/// the source files it cited. Both are byte counts; `counterfactual_bytes` is supplied by the
/// caller (a `Store` lookup) because the store layer owns on-disk file sizes.
#[derive(Debug, Clone, Copy)]
pub struct AnswerImpact {
    /// Bytes Indexa served: the answer text plus the citation lines actually delivered.
    pub served_bytes: u64,
    /// Bytes the cited source files total in full (the "paste the whole file" counterfactual).
    pub counterfactual_bytes: u64,
}

impl AnswerImpact {
    /// Build from an answer's served size + the precomputed counterfactual (cited-file) size.
    pub fn new(served_bytes: u64, counterfactual_bytes: u64) -> Self {
        Self {
            served_bytes,
            counterfactual_bytes,
        }
    }

    /// Whole-percent reduction vs. pasting the cited files in full, clamped to `0..=100`.
    /// Returns `0` when there is no counterfactual (no cited files / empty index) or when
    /// serving was not actually smaller — never a negative or >100 figure that would
    /// overstate the win (the pitch must stay honest at the point of use).
    pub fn saved_percent(&self) -> u8 {
        if self.counterfactual_bytes == 0 || self.served_bytes >= self.counterfactual_bytes {
            return 0;
        }
        let saved = self.counterfactual_bytes - self.served_bytes;
        let pct = ((saved as f64 / self.counterfactual_bytes as f64) * 100.0).round() as u8;
        // Cap at 99: a real answer always serves *something*, so it can never be "100% less"
        // — rounding 99.9% up to 100 would read as "nothing served", which is false.
        pct.min(99)
    }

    /// Whether the readout is worth showing: there were cited files and serving was smaller.
    /// A no-match answer (zero counterfactual) has nothing to claim, so this is `false` there.
    pub fn is_meaningful(&self) -> bool {
        self.counterfactual_bytes > 0 && self.served_bytes < self.counterfactual_bytes
    }

    /// One-line human readout, e.g.:
    /// `"served 4.2 KB vs 1.8 MB of source — 99% less (~450K tokens at ≈4 bytes/token)"`.
    ///
    /// The token estimate uses the same `(saved_bytes / 4)` basis as
    /// `UsageSummary::savings_line` and the web Impact dashboard — one formula, no drift.
    /// The estimate is omitted when there is no saving (degenerate or zero-counterfactual case).
    pub fn human(&self) -> String {
        let pct = self.saved_percent();
        let base = format!(
            "served {} vs {} of source — {}% less to your AI tool",
            human_bytes(self.served_bytes),
            human_bytes(self.counterfactual_bytes),
            pct,
        );
        if pct == 0 {
            return base;
        }
        let tokens = self.counterfactual_bytes.saturating_sub(self.served_bytes) / 4;
        format!(
            "{base} (~{} tokens at \u{2248}4 bytes/token)",
            human_count(tokens)
        )
    }
}

/// Serializes to the wire shape every `ask` surface already emits:
/// `{ "served_bytes", "counterfactual_bytes", "saved_percent" }` — the two stored fields plus
/// the computed `saved_percent()`. This is the single source of truth for the JSON, replacing
/// the per-surface DTOs (CLI `ImpactJson`, web `AskImpact`) that hand-copied these three fields.
/// Field order is fixed (served, counterfactual, saved_percent) to match the prior DTOs byte-for-byte.
impl Serialize for AnswerImpact {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut st = serializer.serialize_struct("AnswerImpact", 3)?;
        st.serialize_field("served_bytes", &self.served_bytes)?;
        st.serialize_field("counterfactual_bytes", &self.counterfactual_bytes)?;
        st.serialize_field("saved_percent", &self.saved_percent())?;
        st.end()
    }
}

/// Record best-effort token-savings telemetry for an `ask` and return its [`AnswerImpact`].
///
/// Collapses the identical "cited paths → counterfactual size → record usage → impact" block the
/// CLI and web `ask` surfaces both ran: `served` = [`served_bytes`] (answer text + delivered
/// citations), the counterfactual is the cited files' full size, and the row is tagged with
/// `surface` (`"cli"`/`"web"`) and an optional conversation `session_id`. A recording failure is
/// logged and swallowed — telemetry must never fail the answer. The caller owns the `Store` open
/// (each surface opens it differently and decides how to handle an open failure).
///
/// The MCP `ask` tool intentionally does **not** use this: it records its own fully-rendered
/// response length as `served` (a different, equally valid measure — see [`served_bytes`]).
pub fn record_ask_impact(
    store: &mut Store,
    surface: &str,
    answer: &Answer,
    session_id: Option<&str>,
) -> AnswerImpact {
    let paths: Vec<&str> = answer.sources.iter().map(|s| s.path.as_str()).collect();
    let counterfactual = store.counterfactual_bytes_for_paths(&paths).unwrap_or(0);
    let served = served_bytes(answer);
    if let Err(e) = store.record_tool_usage_with_basis(
        surface,
        "ask",
        served,
        counterfactual,
        session_id,
        BASIS_ANSWER_CITATIONS,
    ) {
        tracing::debug!("usage telemetry skipped: {e:#}");
    }
    AnswerImpact::new(served, counterfactual)
}

/// Bytes an [`Answer`] delivered: the answer text plus each citation's path, heading, and
/// snippet — exactly what reaches the AI tool. Counting only the answer text would understate
/// what was served and overstate the savings, so citations are included. This is the
/// `served` accounting used by the web + CLI `ask` surfaces; the MCP `ask` tool records its
/// own fully-rendered response length instead (a different but equally reasonable measure).
pub fn served_bytes(answer: &Answer) -> u64 {
    let citations: usize = answer
        .sources
        .iter()
        .map(|s| s.path.len() + s.heading.len() + s.snippet.len())
        .sum();
    (answer.answer.len() + citations) as u64
}

/// One cited file's contribution to the counterfactual: its full on-disk source size — what
/// pasting that whole file into the AI tool would have cost.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactItem {
    pub path: String,
    pub source_bytes: u64,
}

/// The itemized "show the math" breakdown behind an [`AnswerImpact`]: the per-file source sizes
/// that **sum to** `counterfactual_bytes`, plus the served answer size. Built on demand (one extra
/// per-path store lookup via [`Store::counterfactual_sizes_for_paths`]), so surfaces that only want
/// the one-line readout don't pay for it. `items` are in first-seen citation order.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactBreakdown {
    /// Per cited file, its full source size — sums to the aggregate counterfactual.
    pub items: Vec<ImpactItem>,
    /// Bytes Indexa actually served — the `served` figure (answer text + delivered citations),
    /// identical to [`AnswerImpact::served_bytes`] for the same answer.
    pub answer_text_bytes: u64,
}

impl ImpactBreakdown {
    /// Total counterfactual bytes = sum of the per-file source sizes. Equals
    /// [`AnswerImpact::counterfactual_bytes`] for the same answer (the reconciliation invariant).
    pub fn counterfactual_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.source_bytes).sum()
    }

    /// The matching aggregate [`AnswerImpact`] (served vs. the summed counterfactual).
    pub fn impact(&self) -> AnswerImpact {
        AnswerImpact::new(self.answer_text_bytes, self.counterfactual_bytes())
    }

    /// Multi-line "show the math" block for the CLI and MCP surfaces: the cited files sorted
    /// largest-source-first (biggest savings on top), then a served/saved footer. Empty string
    /// when there's nothing meaningful to show (no cited files, or serving wasn't smaller).
    pub fn human_table(&self) -> String {
        let impact = self.impact();
        if !impact.is_meaningful() || self.items.is_empty() {
            return String::new();
        }
        let mut rows: Vec<&ImpactItem> = self.items.iter().collect();
        rows.sort_by_key(|i| std::cmp::Reverse(i.source_bytes));

        let mut out = String::from(
            "Show the math — Indexa served a retrieved slice instead of these whole files:\n",
        );
        for item in rows {
            out.push_str(&format!(
                "  {:>9}  {}\n",
                human_bytes(item.source_bytes),
                item.path
            ));
        }
        let saved = impact
            .counterfactual_bytes
            .saturating_sub(impact.served_bytes);
        out.push_str(&format!(
            "  = {} of source; served {} — {}% less (~{} tokens at \u{2248}4 bytes/token)",
            human_bytes(impact.counterfactual_bytes),
            human_bytes(impact.served_bytes),
            impact.saved_percent(),
            human_count(saved / 4),
        ));
        out
    }
}

/// Build the itemized [`ImpactBreakdown`] for an answer: per-cited-file source sizes + the served
/// size. One extra store lookup; returns an empty breakdown on store error (detail must never fail
/// the answer). The aggregate [`ImpactBreakdown::impact`] reconciles with [`record_ask_impact`].
pub fn ask_impact_breakdown(store: &Store, answer: &Answer) -> ImpactBreakdown {
    let paths: Vec<&str> = answer.sources.iter().map(|s| s.path.as_str()).collect();
    let items = store
        .counterfactual_sizes_for_paths(&paths)
        .unwrap_or_default()
        .into_iter()
        .map(|(path, source_bytes)| ImpactItem { path, source_bytes })
        .collect();
    ImpactBreakdown {
        items,
        answer_text_bytes: served_bytes(answer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qa::{Answer, SourceCitation};

    fn answer_with(text: &str, sources: Vec<(&str, &str, &str)>) -> Answer {
        Answer {
            question: "q".to_owned(),
            answer: text.to_owned(),
            sources: sources
                .into_iter()
                .map(|(p, h, s)| SourceCitation {
                    path: p.to_owned(),
                    heading: h.to_owned(),
                    snippet: s.to_owned(),
                })
                .collect(),
            confidence: None,
            synthesized: true,
            model: None,
        }
    }

    #[test]
    fn saved_percent_is_honest_and_clamped() {
        // 10 served of 1000 → 99% less.
        assert_eq!(AnswerImpact::new(10, 1000).saved_percent(), 99);
        // No counterfactual (no cited files) → 0, never a divide-by-zero or fake win.
        assert_eq!(AnswerImpact::new(10, 0).saved_percent(), 0);
        // Served larger than the source (degenerate) → 0, never negative.
        assert_eq!(AnswerImpact::new(2000, 1000).saved_percent(), 0);
        // Exactly equal → 0 (no saving to claim).
        assert_eq!(AnswerImpact::new(1000, 1000).saved_percent(), 0);
        // A tiny served vs a huge counterfactual rounds toward 100 but is CAPPED at 99 —
        // you always served something, so "100% less" would be a lie.
        assert_eq!(AnswerImpact::new(1, 1_000_000).saved_percent(), 99);
        assert_eq!(AnswerImpact::new(2_781, 5_017_901).saved_percent(), 99);
    }

    #[test]
    fn is_meaningful_gates_the_readout() {
        assert!(AnswerImpact::new(10, 1000).is_meaningful());
        assert!(!AnswerImpact::new(10, 0).is_meaningful()); // no-match answer
        assert!(!AnswerImpact::new(1000, 1000).is_meaningful());
    }

    #[test]
    fn human_reads_naturally() {
        let line = AnswerImpact::new(4_300, 1_887_437).human();
        assert!(line.contains("4.2 KB"), "served: {line}");
        assert!(line.contains("1.8 MB"), "counterfactual: {line}");
        assert!(line.contains("% less"), "percent: {line}");
        // Token estimate is present when there is a saving.
        assert!(line.contains("tokens at"), "token estimate: {line}");
    }

    #[test]
    fn human_token_estimate_matches_savings_line_basis() {
        // The ≈4 bytes/token estimate must be identical to UsageSummary::savings_line's
        // formula so a user can cross-check the per-answer readout against the weekly total.
        // served=1000, counterfactual=9000 → saved=8000 → 8000/4=2000 tokens.
        let impact = AnswerImpact::new(1_000, 9_000);
        let line = impact.human();
        // Expect "~2000 tokens at ≈4 bytes/token" (or "~2.0K" if ≥1000 — 2000 → "2.0K").
        assert!(
            line.contains("2.0K tokens at") || line.contains("2000 tokens at"),
            "token estimate mismatch: {line}"
        );
    }

    #[test]
    fn human_omits_token_estimate_when_no_saving() {
        // saved_percent() == 0 → no token estimate (nothing to claim).
        let line = AnswerImpact::new(1000, 1000).human();
        assert!(
            !line.contains("tokens at"),
            "should not show tokens: {line}"
        );
    }

    #[test]
    fn serializes_to_the_three_field_wire_shape() {
        // Locks the JSON the CLI `AnswerJson` + web `AskResponse`/SSE `done` all emit — field
        // order and the computed `saved_percent` must stay byte-identical to the old DTOs.
        let json = serde_json::to_string(&AnswerImpact::new(10, 1000)).unwrap();
        assert_eq!(
            json,
            r#"{"served_bytes":10,"counterfactual_bytes":1000,"saved_percent":99}"#
        );
    }

    #[test]
    fn served_bytes_counts_answer_plus_citations() {
        let a = answer_with("hello", vec![("/a.rs", "fn x", "let y = 1")]);
        // 5 (answer) + 5 (path) + 4 (heading) + 9 (snippet) = 23
        assert_eq!(served_bytes(&a), 23);
        // No citations → just the answer text.
        assert_eq!(served_bytes(&answer_with("hi", vec![])), 2);
    }

    fn breakdown(items: &[(&str, u64)], served: u64) -> ImpactBreakdown {
        ImpactBreakdown {
            items: items
                .iter()
                .map(|(p, b)| ImpactItem {
                    path: (*p).to_owned(),
                    source_bytes: *b,
                })
                .collect(),
            answer_text_bytes: served,
        }
    }

    #[test]
    fn breakdown_items_sum_to_aggregate_counterfactual() {
        // The reconciliation invariant: per-file source sizes must sum to the aggregate the
        // one-line readout reports, and impact() must equal the same-served AnswerImpact.
        let b = breakdown(&[("/a.rs", 1_000), ("/b.rs", 3_000), ("/dir", 6_000)], 250);
        assert_eq!(b.counterfactual_bytes(), 10_000);
        let agg = b.impact();
        assert_eq!(agg.served_bytes, 250);
        assert_eq!(agg.counterfactual_bytes, 10_000);
        assert_eq!(
            agg.saved_percent(),
            AnswerImpact::new(250, 10_000).saved_percent()
        );
    }

    #[test]
    fn human_table_sorts_largest_first_and_reconciles() {
        let b = breakdown(&[("/small.rs", 1_000), ("/huge.rs", 9_000)], 200);
        let table = b.human_table();
        // Largest source first.
        let huge = table.find("/huge.rs").unwrap();
        let small = table.find("/small.rs").unwrap();
        assert!(huge < small, "largest file must be listed first:\n{table}");
        // Footer carries the summed counterfactual + served + token estimate.
        assert!(table.contains("of source"), "{table}");
        assert!(table.contains("% less"), "{table}");
        assert!(table.contains("tokens at"), "{table}");
    }

    #[test]
    fn human_table_empty_when_not_meaningful() {
        // Served >= counterfactual → nothing honest to show.
        assert!(breakdown(&[("/a.rs", 100)], 500).human_table().is_empty());
        // No cited files → empty.
        assert!(breakdown(&[], 500).human_table().is_empty());
    }
}
