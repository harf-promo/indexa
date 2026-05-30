//! The background summarization queue.

use super::{FailedQueueItem, QueueItem, QueueStats, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

impl Store {
    // ── Summary queue ────────────────────────────────────────────────────────

    /// Enqueue (path, kind, depth) items; ignores duplicates.
    pub fn enqueue_summary_items(&mut self, items: &[(String, String, i64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO summary_queue (path, kind, depth, state)
                 VALUES (?1, ?2, ?3, 'pending')",
            )?;
            for (path, kind, depth) in items {
                stmt.execute(params![path, kind, depth])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically claim one pending item — deepest first (files before their parent dirs).
    ///
    /// Uses a single `UPDATE ... WHERE path = (SELECT ... LIMIT 1) RETURNING` statement so the
    /// select-and-claim is one atomic write. The previous SELECT-then-separate-UPDATE let two
    /// connections (multiple workers + the web summarize path each open their own connection)
    /// read the same pending row before either flipped it, claiming and summarizing it twice.
    /// With WAL + `busy_timeout`, concurrent claims now serialize and each sees the prior claim.
    pub fn next_queue_item(&mut self) -> Result<Option<QueueItem>> {
        let item = self
            .conn
            .query_row(
                "UPDATE summary_queue
                 SET state='in_flight', attempts=attempts+1, updated_at=unixepoch()
                 WHERE path = (
                     SELECT path FROM summary_queue
                     WHERE state='pending'
                     ORDER BY depth DESC LIMIT 1
                 )
                 RETURNING path, kind, depth",
                [],
                |r| {
                    Ok(QueueItem {
                        path: r.get(0)?,
                        kind: r.get(1)?,
                        depth: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(item)
    }

    /// Reset items left `in_flight` by a previously crashed/killed run back to `pending`
    /// so they get retried; items whose `attempts` already reached `max_attempts` are marked
    /// `failed` instead (they keep crashing). Returns `(requeued, failed)`.
    ///
    /// Call this **once at process startup, before any worker begins claiming** — never while
    /// workers are draining, or it would reset an item another worker is actively processing.
    pub fn requeue_stale_in_flight(&mut self, max_attempts: u32) -> Result<(usize, usize)> {
        let tx = self.conn.transaction()?;
        let failed = tx.execute(
            "UPDATE summary_queue
             SET state='failed', error='exceeded max attempts after interruption',
                 updated_at=unixepoch()
             WHERE state='in_flight' AND attempts >= ?1",
            params![max_attempts],
        )?;
        let requeued = tx.execute(
            "UPDATE summary_queue
             SET state='pending', updated_at=unixepoch()
             WHERE state='in_flight'",
            [],
        )?;
        tx.commit()?;
        Ok((requeued, failed))
    }

    /// Mark a queue item's state (e.g. "done" or "failed").
    pub fn mark_queue_state(&mut self, path: &str, state: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE summary_queue SET state=?1, error=?2, updated_at=unixepoch() WHERE path=?3",
            params![state, error, path],
        )?;
        Ok(())
    }

    /// Queue statistics for status display.
    pub fn queue_stats(&self) -> Result<QueueStats> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM summary_queue GROUP BY state")?;
        let mut stats = QueueStats::default();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let state: String = row.get(0)?;
            let n: i64 = row.get(1)?;
            match state.as_str() {
                "pending" => stats.pending = n,
                "in_flight" => stats.in_flight = n,
                "done" => stats.done = n,
                "failed" => stats.failed = n,
                _ => {}
            }
        }
        Ok(stats)
    }

    /// Return up to `limit` items in the `failed` state, with their error messages.
    pub fn failed_queue_items(&self, limit: usize) -> Result<Vec<FailedQueueItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, error FROM summary_queue WHERE state = 'failed' ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(FailedQueueItem {
                path: r.get(0)?,
                error: r.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
