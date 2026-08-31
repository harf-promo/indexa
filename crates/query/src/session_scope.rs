//! Content-based scoping for agent-session transcripts (agent-session-content-scope).
//!
//! `search`/`ask` today can only scope to "agent-session transcript content" via `path:`/
//! `scope` (works only if transcripts live in their own directory) or `ext:jsonl` (matches
//! ANY `.jsonl` file, transcript or not — it filters on the raw extension, not on which parser
//! actually claimed the file). This module closes that gap with a decoupled post-pass: after a
//! scan/deep pass has populated `entries`, re-check every `.jsonl`/`.ndjson` row against the
//! same content-sniffed [`indexa_parsers::agent_sessions::AgentSessionParser`] the deep phase
//! itself uses, and stamp a real match with `entries.hint_cat = "agent-session"`. Query-side,
//! `predicates::category` / `SearchParams::category` then filter on that stamp — see
//! `crates/mcp/src/retrieval.rs`'s `search()`.
//!
//! Deliberately NOT inlined into `apps/indexa/src/commands/deep.rs`'s per-file loop: this is a
//! separate, idempotent pass callable any time (called once after `cmd_deep` returns in the
//! `Deep` CLI arm — fail-open, see `apps/indexa/src/main.rs`).

use indexa_core::store::Store;
use indexa_parsers::agent_sessions::AgentSessionParser;
use indexa_parsers::Parser;
use std::path::Path;

/// The `hint_cat` value stamped on a `.jsonl`/`.ndjson` entry once its content is confirmed to
/// be a Claude Code session transcript.
pub const AGENT_SESSION_CATEGORY: &str = "agent-session";

/// Re-check every `.jsonl`/`.ndjson` entry not already tagged [`AGENT_SESSION_CATEGORY`]
/// against [`AgentSessionParser::accepts_path`] (content-sniff only, never the extension —
/// see that parser's own docs), stamping a real match via [`Store::set_entry_category`].
/// Returns the count of entries newly tagged this call. A file that no longer sniffs as a
/// transcript (or no longer exists) is simply left untagged — `accepts_path` re-reads the file
/// itself, so this is always current as of the call, not a cached decision from scan time.
pub fn tag_agent_session_entries(store: &mut Store) -> anyhow::Result<usize> {
    let candidates = store.jsonl_like_entries_not_tagged(AGENT_SESSION_CATEGORY)?;
    let parser = AgentSessionParser;
    let mut tagged = 0usize;
    for path in candidates {
        if parser.accepts_path(Path::new(&path)) {
            store.set_entry_category(&path, AGENT_SESSION_CATEGORY)?;
            tagged += 1;
        }
    }
    Ok(tagged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::walker::{Entry, EntryKind};

    /// One valid Claude Code transcript line (`"type":"user"` + a string `message.content`) —
    /// enough for `AgentSessionParser::accepts_path`'s content-sniff to accept the file.
    const TRANSCRIPT_LINE: &str =
        r#"{"type":"user","message":{"role":"user","content":"hello there"}}"#;

    fn seed_file(store: &mut Store, path: &std::path::Path) {
        store
            .upsert_entries(&[Entry {
                path: path.to_path_buf(),
                kind: EntryKind::File,
                size: 0,
                modified: None,
                hint: None,
                is_binary: false,
            }])
            .unwrap();
    }

    #[test]
    fn tags_a_real_transcript_and_leaves_an_unrelated_jsonl_file_alone() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("session.jsonl");
        std::fs::write(&transcript, TRANSCRIPT_LINE).unwrap();
        let data_file = root.path().join("data.jsonl");
        std::fs::write(&data_file, r#"{"id":1,"value":"not a transcript"}"#).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_dir.path().join("index.db")).unwrap();
        seed_file(&mut store, &transcript);
        seed_file(&mut store, &data_file);

        let tagged = tag_agent_session_entries(&mut store).unwrap();
        assert_eq!(tagged, 1);

        let cats = store
            .hint_cats_for(&[transcript.to_str().unwrap(), data_file.to_str().unwrap()])
            .unwrap();
        assert_eq!(
            cats.get(transcript.to_str().unwrap()).map(String::as_str),
            Some(AGENT_SESSION_CATEGORY)
        );
        assert!(!cats.contains_key(data_file.to_str().unwrap()));
    }

    #[test]
    fn a_file_already_tagged_is_not_re_processed() {
        // Re-running the pass must not choke on an already-tagged row, and the count reflects
        // only newly-tagged entries.
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("session.jsonl");
        std::fs::write(&transcript, TRANSCRIPT_LINE).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_dir.path().join("index.db")).unwrap();
        seed_file(&mut store, &transcript);

        assert_eq!(tag_agent_session_entries(&mut store).unwrap(), 1);
        assert_eq!(tag_agent_session_entries(&mut store).unwrap(), 0);
    }
}
