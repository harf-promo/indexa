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

/// Pull a usable one-line query out of the model's reply: take the first non-empty
/// line, strip a leading `REWRITTEN:`/`Query:` label and surrounding markdown/quotes,
/// and reject empty or prose-length output (→ caller falls back to the question).
fn clean_rewrite(reply: &str) -> Option<String> {
    for raw in reply.lines() {
        let mut line = raw.trim();
        for label in [
            "REWRITTEN:",
            "Rewritten:",
            "Query:",
            "QUERY:",
            "Rewritten query:",
        ] {
            if let Some(rest) = line.strip_prefix(label) {
                line = rest.trim();
            }
        }
        let line = line.trim_matches(['"', '`', '*', ' ']).trim();
        if line.is_empty() {
            continue;
        }
        if line.chars().count() > REWRITE_MAX_CHARS {
            return None; // not a query — the model wrote prose
        }
        return Some(line.to_owned());
    }
    None
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
        // First non-empty line wins; reasoning preamble is skipped.
        assert_eq!(
            clean_rewrite("\n  \nthe standalone query").as_deref(),
            Some("the standalone query")
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
