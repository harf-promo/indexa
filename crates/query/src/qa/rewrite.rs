//! Follow-up query rewriting for conversational Ask: turn a context-dependent
//! follow-up ("and what about X?", "why is that?") into a self-contained retrieval
//! query using the conversation so far.
//!
//! **History-gated and fail-open.** Empty history skips the LLM call entirely, so a
//! single-shot Ask incurs zero added latency. A blank / echoed / errored reply falls
//! back to the original question — retrieval then runs on the literal follow-up, which
//! is no worse than today's stateless behavior.

use indexa_llm::Generator;

use super::PriorTurn;

/// How many trailing conversation turns to show the rewriter, and how far to truncate
/// each prior answer in the rewrite prompt (the answer is only there to resolve
/// pronouns — the full text would bloat the call for no gain).
const REWRITE_CONTEXT_TURNS: usize = 4;
const REWRITE_ANSWER_TRUNC: usize = 400;
/// Reject a "rewrite" longer than this — the model dumped prose instead of a query.
const REWRITE_MAX_CHARS: usize = 400;

/// The query that retrieval should embed + search. With no history this is just the
/// question; with history it is the LLM-reformulated standalone query (fail-open).
pub(crate) async fn resolve_search_query(
    llm: &dyn Generator,
    question: &str,
    history: &[PriorTurn],
) -> String {
    if history.is_empty() {
        return question.to_owned();
    }
    match llm.generate(&rewrite_prompt(question, history)).await {
        Ok(reply) => clean_rewrite(&reply).unwrap_or_else(|| question.to_owned()),
        Err(_) => question.to_owned(),
    }
}

fn rewrite_prompt(question: &str, history: &[PriorTurn]) -> String {
    let start = history.len().saturating_sub(REWRITE_CONTEXT_TURNS);
    let mut convo = String::new();
    for t in &history[start..] {
        convo.push_str("Q: ");
        convo.push_str(t.question.trim());
        convo.push_str("\nA: ");
        convo.push_str(truncate(t.answer.trim(), REWRITE_ANSWER_TRUNC));
        convo.push('\n');
    }
    format!(
        "Given the conversation so far and a follow-up question, rewrite the follow-up \
         into a single self-contained search query that needs no prior context. Resolve \
         pronouns and ellipsis using the conversation. If the follow-up is already \
         self-contained, return it unchanged. Reply with ONLY the rewritten query on one \
         line, nothing else.\n\
         \n\
         CONVERSATION SO FAR:\n\
         {convo}\n\
         FOLLOW-UP: {question}\n\
         REWRITTEN:"
    )
}

/// Leading labels the model may prefix the rewritten query with — stripped before use.
const REWRITE_LABELS: [&str; 5] = [
    "REWRITTEN:",
    "Rewritten:",
    "Query:",
    "QUERY:",
    "Rewritten query:",
];

/// Chatty preamble openers — a line starting with one of these is an introduction
/// ("Sure, here's the query:", "Okay, the standalone question is …"), not the query
/// itself, so it is skipped. Lowercased prefix match.
const REWRITE_PREAMBLE_OPENERS: [&str; 8] = [
    "sure",
    "here",
    "here's",
    "okay",
    "ok,",
    "of course",
    "the standalone",
    "certainly",
];

/// Pull a usable one-line query out of the model's reply. Strips any `REWRITTEN:`/`Query:`
/// label and surrounding markdown/quotes from each line, skips obvious chatty preamble lines
/// (an intro that ends in `:` or opens with "Sure"/"Here"/"Okay"/…), and returns the first
/// remaining query-like line. Rejects empty or prose-length output → the caller falls back to
/// the original question (fail-open).
fn clean_rewrite(reply: &str) -> Option<String> {
    for raw in reply.lines() {
        let Some(line) = clean_rewrite_line(raw) else {
            continue;
        };
        if line.chars().count() > REWRITE_MAX_CHARS {
            // This line is prose, not a query — keep scanning for a tighter one
            // instead of failing the whole reply (the query often follows the preamble).
            continue;
        }
        return Some(line);
    }
    None
}

/// Normalize a single reply line into a candidate query, or `None` if it's empty,
/// pure label, or a chatty preamble/introduction line (which precedes the real query).
fn clean_rewrite_line(raw: &str) -> Option<String> {
    let mut line = raw.trim();
    for label in REWRITE_LABELS {
        if let Some(rest) = line.strip_prefix(label) {
            line = rest.trim();
        }
    }
    let line = line.trim_matches(['"', '`', '*', ' ', '\t']).trim();
    if line.is_empty() {
        return None;
    }
    // An introductory line that announces the query (e.g. "Sure, here's the standalone
    // query:") rather than being one. Two tells: it ends in a colon (introducing what
    // follows) or it opens with a conversational filler word.
    let lower = line.to_ascii_lowercase();
    let is_preamble = line.ends_with(':')
        || REWRITE_PREAMBLE_OPENERS
            .iter()
            .any(|p| lower.starts_with(p));
    if is_preamble {
        return None;
    }
    Some(line.to_owned())
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_rewrite_strips_label_and_quotes() {
        assert_eq!(
            clean_rewrite("REWRITTEN: how does retrieval rank hits").as_deref(),
            Some("how does retrieval rank hits")
        );
        assert_eq!(
            clean_rewrite("\"what is the archive penalty\"").as_deref(),
            Some("what is the archive penalty")
        );
        // First usable line wins; blank lines are skipped.
        assert_eq!(
            clean_rewrite("\n  \nhow does retrieval rank hits").as_deref(),
            Some("how does retrieval rank hits")
        );
    }

    #[test]
    fn clean_rewrite_drops_chatty_preamble_and_keeps_the_query() {
        // The model prefixed a conversational intro line; the real query follows it.
        assert_eq!(
            clean_rewrite("Sure, here's the standalone query:\nhow does MMR diversity work")
                .as_deref(),
            Some("how does MMR diversity work")
        );
        // A trailing-colon introducer on its own line is also skipped.
        assert_eq!(
            clean_rewrite("The standalone question is:\nwhat is the archive penalty").as_deref(),
            Some("what is the archive penalty")
        );
        // An "Okay, …" opener line is skipped in favor of the following query.
        assert_eq!(
            clean_rewrite("Okay.\nwhere is path_is_historical defined").as_deref(),
            Some("where is path_is_historical defined")
        );
    }

    #[test]
    fn clean_rewrite_passes_through_a_clean_query_unchanged() {
        // A plain one-line rewrite with no preamble is returned verbatim.
        assert_eq!(
            clean_rewrite("how is the archive penalty configured").as_deref(),
            Some("how is the archive penalty configured")
        );
    }

    #[test]
    fn clean_rewrite_rejects_empty_and_prose() {
        assert_eq!(clean_rewrite("   \n  "), None);
        let prose = "x ".repeat(REWRITE_MAX_CHARS + 5);
        assert_eq!(clean_rewrite(&prose), None);
    }

    #[test]
    fn rewrite_prompt_uses_recent_turns_only() {
        let history: Vec<PriorTurn> = (0..6)
            .map(|i| PriorTurn {
                question: format!("q{i}"),
                answer: format!("a{i}"),
            })
            .collect();
        let p = rewrite_prompt("follow up", &history);
        // Only the last REWRITE_CONTEXT_TURNS turns appear.
        assert!(!p.contains("q1"));
        assert!(p.contains("q2"));
        assert!(p.contains("q5"));
        assert!(p.contains("FOLLOW-UP: follow up"));
    }
}
