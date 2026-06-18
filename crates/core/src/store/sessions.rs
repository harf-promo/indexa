//! Conversational Ask sessions: ordered Q&A turns grouped by a session id
//! (`ask_sessions` + `conversation_turns`). A follow-up question is resolved
//! against, and answered with, the prior turns of its session.
//!
//! Like `saved_queries` / `decisions`, a conversation is standing user state: it
//! survives entry removal and is not cleared by the `entries.rs` delete paths.

use super::Store;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// One persisted turn of a conversation. `sources_json` is kept opaque at the
/// store layer (the surface layers serialize/deserialize their own citation shape).
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub turn_index: i64,
    pub question: String,
    pub answer: String,
    pub sources_json: String,
    pub created_at: i64,
}

impl Store {
    /// Create the session row if absent; a no-op (does not reset `created_at`) if it
    /// already exists. Idempotent so callers can send the same id on every turn.
    pub fn ensure_session(&mut self, id: &str, scope: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ask_sessions (id, scope) VALUES (?1, ?2)
             ON CONFLICT(id) DO NOTHING",
            params![id, scope],
        )?;
        Ok(())
    }

    /// Append the next turn to a session, computing `turn_index` as `MAX+1` (0 for the
    /// first turn) in one statement, and bump the session's `updated_at`. Returns the
    /// new turn's index. The caller is expected to have `ensure_session`'d first.
    pub fn append_turn(
        &mut self,
        session_id: &str,
        question: &str,
        answer: &str,
        sources_json: &str,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO conversation_turns (session_id, turn_index, question, answer, sources_json)
             SELECT ?1,
                    COALESCE((SELECT MAX(turn_index) + 1 FROM conversation_turns WHERE session_id = ?1), 0),
                    ?2, ?3, ?4",
            params![session_id, question, answer, sources_json],
        )?;
        tx.execute(
            "UPDATE ask_sessions SET updated_at = unixepoch() WHERE id = ?1",
            params![session_id],
        )?;
        let idx: i64 = tx.query_row(
            "SELECT MAX(turn_index) FROM conversation_turns WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(idx)
    }

    /// The most recent `n` turns of a session, returned in chronological order
    /// (oldest first) so callers can fold them into a prompt directly.
    pub fn recent_turns(&self, session_id: &str, n: usize) -> Result<Vec<ConversationTurn>> {
        let mut stmt = self.conn.prepare(
            "SELECT turn_index, question, answer, sources_json, created_at
             FROM conversation_turns WHERE session_id = ?1
             ORDER BY turn_index DESC LIMIT ?2",
        )?;
        let mut rows = stmt
            .query_map(params![session_id, n as i64], row_to_turn)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.reverse(); // DESC LIMIT n → chronological
        Ok(rows)
    }

    /// Every turn of a session in chronological order (for `--show` / debugging).
    pub fn turns_for_session(&self, session_id: &str) -> Result<Vec<ConversationTurn>> {
        let mut stmt = self.conn.prepare(
            "SELECT turn_index, question, answer, sources_json, created_at
             FROM conversation_turns WHERE session_id = ?1
             ORDER BY turn_index ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], row_to_turn)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether a session row exists.
    pub fn session_exists(&self, id: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM ask_sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }
}

fn row_to_turn(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationTurn> {
    Ok(ConversationTurn {
        turn_index: r.get(0)?,
        question: r.get(1)?,
        answer: r.get(2)?,
        sources_json: r.get(3)?,
        created_at: r.get(4)?,
    })
}
