//! Per-answer "impact": how much smaller the served context was than the cited source files —
//! the concrete, per-query proof of Indexa's "retrieve the slice, don't pack the repo" pitch.

use indexa_core::text::human_bytes;

use crate::qa::Answer;

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

    /// One-line human readout, e.g. "served 4.2 KB vs 1.8 MB of source — 99% less to your AI tool".
    pub fn human(&self) -> String {
        format!(
            "served {} vs {} of source — {}% less to your AI tool",
            human_bytes(self.served_bytes),
            human_bytes(self.counterfactual_bytes),
            self.saved_percent(),
        )
    }
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
    }

    #[test]
    fn served_bytes_counts_answer_plus_citations() {
        let a = answer_with("hello", vec![("/a.rs", "fn x", "let y = 1")]);
        // 5 (answer) + 5 (path) + 4 (heading) + 9 (snippet) = 23
        assert_eq!(served_bytes(&a), 23);
        // No citations → just the answer text.
        assert_eq!(served_bytes(&answer_with("hi", vec![])), 2);
    }
}
