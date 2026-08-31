//! Claude Code agent session-transcript parser (opt-in — never auto-discovered, never scanned
//! by default; see `docs/how-to/index-agent-session-history.md`).
//!
//! Claude Code writes one JSON object per line to `~/.claude/projects/<project>/<session>.jsonl`.
//! Conversation turns carry `"type":"user"` or `"type":"assistant"` with a `message` object whose
//! `content` is either a plain string (user turns) or an array of typed blocks (assistant turns,
//! and occasionally user turns holding a tool result): `{"type":"text","text":"..."}` interleaved
//! with `{"type":"tool_use",...}` / `{"type":"tool_result",...}` / `{"type":"thinking",...}`
//! blocks. This parser extracts only the human-readable prose — `text` blocks, or a user turn's
//! plain string — and skips everything else: tool-call/tool-result payloads, thinking blocks, and
//! any embedded image/base64 data.
//!
//! Real transcripts also interleave session-metadata lines with no conversational content at all
//! (`"last-prompt"`, `"queue-operation"`, `"mode"`, `"permission-mode"`, `"file-history-snapshot"`,
//! …, confirmed by hand against a couple of lines of real files on this machine — never indexed or
//! committed). `parse` simply skips any line whose `type` isn't `"user"`/`"assistant"` or whose
//! `message.content` yields no prose; see [`sniff_transcript`] for how `accepts_path` tells a real
//! transcript apart from an arbitrary JSON/JSONL file without relying on the extension.

use crate::types::{chunk_words, ChunkParams, Extracted, Parser};
use anyhow::Result;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// The only two line `type`s this parser treats as conversation turns.
const TURN_TYPES: &[&str] = &["user", "assistant"];

/// Bound on how many leading lines [`sniff_transcript`] will read looking for one matching
/// turn-shaped line.
const SNIFF_MAX_LINES: usize = 50;
/// Bound on how many leading bytes [`sniff_transcript`] will read, independent of line count —
/// a pathological single huge leading line (or line count) can't make the sniff read further
/// than this into the file.
const SNIFF_MAX_BYTES: u64 = 256 * 1024;

pub struct AgentSessionParser;

impl Parser for AgentSessionParser {
    /// Content-sniff only — see [`sniff_transcript`]. Never dispatches on the `.jsonl` extension
    /// alone, so an unrelated JSONL data file falls through to the generic JSON/text parser.
    fn accepts_path(&self, path: &Path) -> bool {
        sniff_transcript(path).unwrap_or(false)
    }

    /// Never claimed by MIME — `accepts_path` is the sole (content-sniffed) entry point.
    fn accepts_mime(&self, _mime: &str) -> bool {
        false
    }

    fn parse(&self, path: &Path) -> Result<Extracted> {
        self.parse_chunked(path, ChunkParams::default())
    }

    fn parse_chunked(&self, path: &Path, chunk: ChunkParams) -> Result<Extracted> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut chunks = Vec::new();
        let mut seq = 0usize;
        let mut turn_no = 0usize;

        // Stream line-by-line rather than reading the whole file: transcripts can run to many
        // GB, and only one line is ever resident at a time.
        for line in reader.lines() {
            // A non-UTF8 line (or any other read error) is skipped, not fatal — best-effort,
            // matching this crate's lossy-fallback convention: one bad line must not lose the
            // rest of a large transcript.
            let Ok(line) = line else { continue };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(turn_type) = value.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            if !TURN_TYPES.contains(&turn_type) {
                continue;
            }
            let Some(prose) = turn_prose(&value) else {
                continue;
            };
            if prose.trim().is_empty() {
                continue;
            }
            turn_no += 1;
            let heading = format!("Turn {turn_no} [{turn_type}]");
            chunk_words(
                path,
                &prose,
                &heading,
                None,
                chunk.size,
                chunk.overlap,
                &mut seq,
                &mut chunks,
            );
        }

        Ok(Extracted {
            source: path.to_path_buf(),
            mime: "application/x-ndjson".into(),
            chunks,
            edges: Vec::new(),
        })
    }
}

/// Extract one turn's human-readable prose from a parsed `{"type":"user"|"assistant", "message":
/// {...}, ...}` line. A user turn's `message.content` is typically a plain string; an assistant
/// turn's is an array of blocks (occasionally a user turn's is too, e.g. a tool-result reply) —
/// only `{"type":"text","text":"..."}` blocks are kept, joined with a blank line between them.
/// Returns `None` when the shape doesn't match (so the caller skips the line) or the turn carries
/// no prose at all (e.g. an assistant turn that is pure `tool_use`, or a user turn that is pure
/// `tool_result`).
fn turn_prose(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    }
}

/// Bounded content sniff for `accepts_path`: scans up to [`SNIFF_MAX_LINES`] leading lines (or
/// [`SNIFF_MAX_BYTES`], whichever budget is exhausted first) for one line that parses as JSON with
/// a `"type"` in [`TURN_TYPES`] and a `"message"` object carrying a `"content"` field shaped like
/// a real turn (a string, or an array of blocks).
///
/// Scanning a *window* rather than strictly the first non-empty line is deliberate: real
/// transcripts observed on this machine (never indexed/committed) commonly open with several
/// lines of session metadata (`"last-prompt"`, `"queue-operation"`, `"mode"`, …) that have no
/// `message` field at all — sniffing only the literal first line would misclassify most real
/// session files as "not a transcript" and silently fall back to the generic JSON/text parser.
/// The window is still small and byte-capped, so this stays a cheap sniff, not a full parse, and
/// can't be tricked into reading a huge file in full.
fn sniff_transcript(path: &Path) -> std::io::Result<bool> {
    let file = std::fs::File::open(path)?;
    let bounded = file.take(SNIFF_MAX_BYTES);
    let reader = BufReader::new(bounded);
    for line in reader.lines().take(SNIFF_MAX_LINES) {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line_matches_turn_shape(line) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Does one line parse as JSON shaped like a real transcript turn? See [`sniff_transcript`].
fn line_matches_turn_shape(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let Some(turn_type) = value.get("type").and_then(|t| t.as_str()) else {
        return false;
    };
    if !TURN_TYPES.contains(&turn_type) {
        return false;
    }
    let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
        return false;
    };
    matches!(
        content,
        serde_json::Value::String(_) | serde_json::Value::Array(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handful of synthetic lines shaped like a real Claude Code transcript, interleaving
    /// session-metadata lines (no `message` field) with `user`/`assistant` turns, and an
    /// assistant turn whose content mixes `thinking`/`tool_use`/`text` blocks. Entirely
    /// fabricated — no content from any real `~/.claude/projects/**/*.jsonl` file.
    fn fixture_transcript() -> String {
        [
            r#"{"type":"last-prompt","sessionId":"abc","timestamp":"2026-08-30T00:00:00Z"}"#,
            r#"{"type":"mode","mode":"default"}"#,
            r#"{"type":"user","message":{"role":"user","content":"did I already investigate the flaky retry test?"},"uuid":"u1"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me check"},{"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"flaky"}}]},"uuid":"a1"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"no matches"}]},"uuid":"u2"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Not yet — I searched for \"flaky\" and found nothing. Want me to look under a different name?"}]},"uuid":"a2"}"#,
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            "",
            r#"{"type":"user","message":{"role":"user","content":"yes, try \"intermittent\" instead"},"uuid":"u3"}"#,
        ]
        .join("\n")
    }

    #[test]
    fn accepts_a_synthetic_transcript_even_when_first_lines_are_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        std::fs::write(&p, fixture_transcript()).unwrap();
        assert!(
            AgentSessionParser.accepts_path(&p),
            "must recognize a transcript whose leading lines are session metadata, not just a \
             literal first-line user/assistant turn"
        );
    }

    #[test]
    fn rejects_plain_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"name":"indexa","version":"0.78.0"}"#).unwrap();
        assert!(!AgentSessionParser.accepts_path(&p));
    }

    #[test]
    fn rejects_unrelated_jsonl_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        let lines = [
            r#"{"type":"user","event":"signup","user_id":42}"#,
            r#"{"type":"assistant","event":"reply_sent","channel":"email"}"#,
        ]
        .join("\n");
        std::fs::write(&p, lines).unwrap();
        assert!(
            !AgentSessionParser.accepts_path(&p),
            "a `type: user/assistant` value that lacks a shaped `message` field must not match"
        );
    }

    #[test]
    fn rejects_non_json_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.txt");
        std::fs::write(&p, "just some plain notes, not JSON at all\nline two").unwrap();
        assert!(!AgentSessionParser.accepts_path(&p));
    }

    #[test]
    fn rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.jsonl");
        std::fs::write(&p, "").unwrap();
        assert!(!AgentSessionParser.accepts_path(&p));
    }

    #[test]
    fn parse_extracts_turns_in_order_and_skips_tool_and_thinking_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        std::fs::write(&p, fixture_transcript()).unwrap();

        let ex = AgentSessionParser.parse(&p).unwrap();
        assert!(!ex.chunks.is_empty());

        let all_text: Vec<&str> = ex.chunks.iter().map(|c| c.text.as_str()).collect();
        let joined = all_text.join(" | ");

        // The four turns with prose all made it through, in order.
        assert!(joined.contains("did I already investigate"));
        assert!(joined.contains("Not yet"));
        assert!(joined.contains("intermittent"));

        // Tool-call/tool-result/thinking payloads never leak into a chunk.
        assert!(!joined.contains("let me check"), "thinking block leaked");
        assert!(!joined.contains("Grep"), "tool_use block leaked");
        assert!(!joined.contains("no matches"), "tool_result block leaked");

        // Headings identify the speaker per turn.
        assert!(ex.chunks[0].heading.contains("[user]"));
        assert!(ex.chunks.iter().any(|c| c.heading.contains("[assistant]")));

        // Only the three turns that actually carry prose are numbered/emitted — the pure
        // tool_use assistant turn and the pure tool_result user turn contribute no chunk, so
        // "Turn 2 [assistant]" (not "Turn 4") is the *second* turn with a text block.
        let headings: Vec<&str> = ex.chunks.iter().map(|c| c.heading.as_str()).collect();
        assert_eq!(
            headings,
            vec!["Turn 1 [user]", "Turn 2 [assistant]", "Turn 3 [user]"],
            "prose-less turns must not consume a turn number or emit a chunk"
        );
    }

    #[test]
    fn parse_skips_malformed_lines_without_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"first turn"}}"#,
            "not even json",
            r#"{"type":"user"}"#, // missing `message` entirely
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second turn"}]}}"#,
        ]
        .join("\n");
        std::fs::write(&p, lines).unwrap();

        let ex = AgentSessionParser.parse(&p).unwrap();
        let joined = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(joined.contains("first turn"));
        assert!(joined.contains("second turn"));
    }

    #[test]
    fn parse_skips_non_utf8_line_without_erroring_on_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            br#"{"type":"user","message":{"role":"user","content":"before the bad line"}}"#,
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(b"\xFF\xFE not valid utf8 on its own line");
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"user","message":{"role":"user","content":"after the bad line"}}"#,
        );
        std::fs::write(&p, &bytes).unwrap();

        let ex = AgentSessionParser.parse(&p).unwrap();
        let joined = ex
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(joined.contains("before the bad line"));
        assert!(joined.contains("after the bad line"));
    }

    #[test]
    fn parse_handles_array_content_on_a_user_turn() {
        // A user turn's content can itself be an array (e.g. a tool-result reply) — no text
        // block present, so it must contribute nothing rather than error.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.jsonl");
        std::fs::write(
            &p,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        )
        .unwrap();
        let ex = AgentSessionParser.parse(&p).unwrap();
        assert!(ex.chunks.is_empty());
    }

    #[test]
    fn declared_mime_is_never_claimed() {
        assert!(!AgentSessionParser.accepts_mime("application/x-ndjson"));
        assert!(!AgentSessionParser.accepts_mime("application/json"));
    }
}
