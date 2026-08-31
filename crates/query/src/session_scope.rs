//! Content-based scoping for agent-session transcripts (agent-session-content-scope).
//!
//! `search`/`ask` today can only scope to "agent-session transcript content" via `path:`/
//! `scope` (works only if transcripts live in their own directory) or `ext:jsonl` (matches
//! ANY `.jsonl` file, transcript or not — it filters on the raw extension, not on which parser
//! actually claimed the file). This module closes that gap with a decoupled post-pass: after a
//! scan/deep pass has populated `entries`, re-check every `.jsonl`/`.ndjson` row against the
//! same content-sniffed [`indexa_parsers::agent_sessions::AgentSessionParser`] the deep phase
//! itself uses, and stamp a real match via [`Store::set_agent_session_flag`]. Query-side,
//! `predicates::category` / `SearchParams::category` then filter on that stamp — see
//! `crates/mcp/src/retrieval.rs`'s `search()`.
//!
//! **Deliberately its own `entries.agent_session` column, NOT `entries.hint_cat`** — the
//! technical scan-time classification column `surface.rs` sets (`.jsonl`/`.ndjson` → `"data"`).
//! `hint_cat` is unconditionally overwritten by
//! `upsert_entries_with_generation`'s `ON CONFLICT DO UPDATE SET hint_cat = excluded.hint_cat`
//! on every rescan/watch-upsert, so a content-derived tag stamped there would get silently
//! cleared back to `"data"` by the next plain `indexa scan` or filesystem-watch event, until the
//! next tagging pass happened to run again. See `entries.agent_session`'s doc comment in
//! `store::schema`'s base DDL for the full column contract (tri-state NULL/0/1).
//!
//! Deliberately NOT inlined into `apps/indexa/src/commands/deep.rs`'s per-file loop: this is a
//! separate, idempotent pass callable any time — called once at the end of every real
//! `cmd_deep` run (fail-open, see `apps/indexa/src/commands/deep.rs`), so every caller of
//! `cmd_deep` (`indexa deep`, `indexa index`, `indexa notes add`, `indexa pack refresh`) gets it
//! for free; the web UI's separate `crates/web/src/jobs_exec/deep.rs` job calls it too.

use indexa_core::store::Store;
use indexa_parsers::agent_sessions::AgentSessionParser;
use indexa_parsers::Parser;
use std::path::Path;

/// The category name `search`'s `category:`/`category` param matches against to mean "confirmed
/// agent-session transcript" — the query-facing name for what `entries.agent_session = 1` means.
/// Not a `hint_cat` value (see the module doc comment above).
pub const AGENT_SESSION_CATEGORY: &str = "agent-session";

/// Re-check every `.jsonl`/`.ndjson` entry not yet content-checked
/// ([`Store::jsonl_like_entries_needing_agent_session_check`]) against
/// [`AgentSessionParser::accepts_path`] (content-sniff only, never the extension — see that
/// parser's own docs), stamping the outcome via [`Store::set_agent_session_flag`] either way —
/// so a rejected non-transcript file is never re-offered as a candidate. Returns the count of
/// entries newly CONFIRMED as transcripts this call (rejections aren't counted, matching the
/// old behavior). A file that no longer sniffs as a transcript is simply flagged `false` —
/// `accepts_path` re-reads the file itself, so this is always current as of the call, not a
/// cached decision from scan time.
pub fn tag_agent_session_entries(store: &mut Store) -> anyhow::Result<usize> {
    let candidates = store.jsonl_like_entries_needing_agent_session_check()?;
    let parser = AgentSessionParser;
    let mut tagged = 0usize;
    for path in candidates {
        let is_transcript = parser.accepts_path(Path::new(&path));
        store.set_agent_session_flag(&path, is_transcript)?;
        if is_transcript {
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

        let confirmed = store
            .agent_session_tagged_paths(&[
                transcript.to_str().unwrap(),
                data_file.to_str().unwrap(),
            ])
            .unwrap();
        assert!(confirmed.contains(transcript.to_str().unwrap()));
        assert!(!confirmed.contains(data_file.to_str().unwrap()));
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

    #[test]
    fn a_rejected_non_transcript_is_not_re_checked_on_the_next_pass() {
        // Regression for the "candidate query never shrinks for a rejected file" gap: a
        // `.jsonl` file that content-sniffs as NOT a transcript must be flagged `false` (not
        // left NULL), so it stops being a candidate on subsequent passes instead of being
        // re-parsed and re-rejected forever.
        let root = tempfile::tempdir().unwrap();
        let data_file = root.path().join("data.jsonl");
        std::fs::write(&data_file, r#"{"id":1,"value":"not a transcript"}"#).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_dir.path().join("index.db")).unwrap();
        seed_file(&mut store, &data_file);

        assert_eq!(tag_agent_session_entries(&mut store).unwrap(), 0);
        assert!(
            !store
                .jsonl_like_entries_needing_agent_session_check()
                .unwrap()
                .contains(&data_file.to_str().unwrap().to_string()),
            "a rejected file must be marked checked, not left as a perpetual candidate"
        );
    }

    #[test]
    fn a_plain_rescan_does_not_clear_the_agent_session_tag() {
        // Regression for the hint_cat-collision bug: `tag_agent_session_entries` stamps a
        // dedicated `entries.agent_session` column specifically so a later plain rescan/
        // watch-upsert (re-`upsert_entries` on the SAME path — exactly what `indexa scan` or a
        // filesystem-watch event does) can never silently clobber the tag the way overloading
        // `hint_cat` used to.
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("session.jsonl");
        std::fs::write(&transcript, TRANSCRIPT_LINE).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_dir.path().join("index.db")).unwrap();
        seed_file(&mut store, &transcript);
        assert_eq!(tag_agent_session_entries(&mut store).unwrap(), 1);

        // Simulate a plain rescan re-upserting the same path (no hint provided, same as a
        // technical-classification-free watch upsert).
        seed_file(&mut store, &transcript);

        let confirmed = store
            .agent_session_tagged_paths(&[transcript.to_str().unwrap()])
            .unwrap();
        assert!(
            confirmed.contains(transcript.to_str().unwrap()),
            "a rescan must not clear the agent-session tag"
        );
    }
}
