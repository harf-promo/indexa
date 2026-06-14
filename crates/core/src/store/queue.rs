//! The background summarization queue.

use super::{FailedQueueItem, QueueItem, QueueStats, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

impl Store {
    // ── Summary queue ────────────────────────────────────────────────────────

    /// Enqueue (path, kind, depth) items; ignores duplicates.
    ///
    /// Paths that are not a live `entries` row are skipped (when the index has entries):
    /// a queue row with no entry can never summarize and would only inflate the backlog —
    /// the historical source of build-artifact rows stuck `pending` forever. The guard is
    /// bypassed for an entry-less index (the legitimate `deep`/`summarize`-without-`scan`
    /// workflow, where queue paths intentionally have no `entries` row).
    pub fn enqueue_summary_items(&mut self, items: &[(String, String, i64)]) -> Result<()> {
        let has_entries = self.entry_count()? > 0;
        let tx = self.conn.transaction()?;
        {
            let mut is_entry = tx.prepare_cached("SELECT 1 FROM entries WHERE path = ?1")?;
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO summary_queue (path, kind, depth, state)
                 VALUES (?1, ?2, ?3, 'pending')",
            )?;
            for (path, kind, depth) in items {
                if has_entries && !is_entry.exists(params![path])? {
                    continue; // not a live entry — don't queue un-processable work
                }
                stmt.execute(params![path, kind, depth])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove a queue row entirely. Used to self-clean an orphaned item whose path is no
    /// longer a live entry (a build artifact that got skipped, or a deleted file) when the
    /// drain claims it — see `process_queue_item_with_passes`. Returns rows removed.
    pub fn delete_queue_item(&mut self, path: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM summary_queue WHERE path = ?1", params![path])?)
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

    /// True if any item strictly deeper inside `dir_path`'s subtree is still `pending`
    /// or `in_flight`.
    ///
    /// The summarize loop uses this to **defer** a directory roll-up (re-enqueue it
    /// `pending`) until its children are summarized — so a concurrent worker can't roll
    /// the directory up from an incomplete child set and mark it `done`. This is a
    /// read-only check that does NOT touch the atomic claim in
    /// [`next_queue_item`](Self::next_queue_item). The `|| '/'` trailing slash guards
    /// prefix-siblings (`/proj` must not match `/projector/x`).
    pub fn subtree_has_unfinished(&self, dir_path: &str, dir_depth: i64) -> Result<bool> {
        let unfinished: bool = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM summary_queue
                 WHERE state IN ('pending','in_flight')
                   AND depth > ?2
                   AND substr(path, 1, length(?1) + 1) = ?1 || '/'
             )",
            params![dir_path, dir_depth],
            |r| r.get(0),
        )?;
        Ok(unfinished)
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

    /// Re-enqueue a just-claimed item as `pending` and **undo the claim's `attempts++`**
    /// (a defer is not a summarization attempt). The summarize loop calls this when it
    /// defers a directory whose children aren't summarized yet, so repeated defers don't
    /// inflate `attempts` and cause `requeue_stale_in_flight` to wrongly fail the dir after
    /// a crash. Floors `attempts` at 0.
    pub fn defer_queue_item(&mut self, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE summary_queue
             SET state='pending', attempts=MAX(0, attempts - 1), updated_at=unixepoch()
             WHERE path=?1",
            params![path],
        )?;
        Ok(())
    }

    /// Mark a queue item's state (e.g. "done" or "failed").
    pub fn mark_queue_state(&mut self, path: &str, state: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE summary_queue SET state=?1, error=?2, updated_at=unixepoch() WHERE path=?3",
            params![state, error, path],
        )?;
        Ok(())
    }

    /// Enqueue a path for (re-)summarization, resetting an existing `pending`/`done`/
    /// `failed` row back to `pending`.
    ///
    /// Neither existing primitive covers a *changed* file: [`enqueue_summary_items`]
    /// uses `INSERT OR IGNORE` (can't reset a `done`/`failed` row) and
    /// [`mark_queue_state`] no-ops when no row exists. This upsert does both — a new
    /// path is inserted `pending`; an existing one is flipped back to `pending` with
    /// `attempts`/`error` cleared so it gets fresh retries. Used by `indexa watch` to
    /// re-queue an edited file and its ancestor directory roll-ups for the worker.
    ///
    /// An **`in_flight`** row is deliberately left untouched: resetting it would let a
    /// second worker re-claim a path a first worker is already summarizing — exactly the
    /// double-claim that [`next_queue_item`](Self::next_queue_item)'s atomic claim
    /// prevents. (A crashed worker's stuck `in_flight` row is recovered separately by
    /// [`requeue_stale_in_flight`](Self::requeue_stale_in_flight) at startup.) The cost:
    /// an edit landing mid-summary isn't re-queued by *that* edit — the next edit, or a
    /// later `deep`/`summarize`, picks it up.
    pub fn mark_for_resummary(&mut self, path: &str, kind: &str, depth: i64) -> Result<()> {
        // Don't (re-)queue a path that isn't a live entry — e.g. a watch event for a file
        // under a skipped build/VCS dir. Such a row could never summarize and would only
        // inflate the queue. Bypassed for an entry-less index (entries are optional).
        if !self.entry_exists(path)? && self.entry_count()? > 0 {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO summary_queue (path, kind, depth, state, attempts, error)
             VALUES (?1, ?2, ?3, 'pending', 0, NULL)
             ON CONFLICT(path) DO UPDATE SET
                 state='pending', attempts=0, error=NULL, updated_at=unixepoch()
                 WHERE summary_queue.state <> 'in_flight'",
            params![path, kind, depth],
        )?;
        Ok(())
    }

    /// Batched [`mark_for_resummary`](Self::mark_for_resummary): one transaction for
    /// the whole set (per-row autocommit would pay a commit per path on a large
    /// refresh). Same semantics — `in_flight` rows are left untouched. Used by the
    /// incremental re-summarize path to re-pend changed files + their stale ancestor
    /// roll-ups in one shot.
    pub fn mark_for_resummary_batch(&mut self, items: &[(String, String, i64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO summary_queue (path, kind, depth, state, attempts, error)
                 VALUES (?1, ?2, ?3, 'pending', 0, NULL)
                 ON CONFLICT(path) DO UPDATE SET
                     state='pending', attempts=0, error=NULL, updated_at=unixepoch()
                     WHERE summary_queue.state <> 'in_flight'",
            )?;
            for (path, kind, depth) in items {
                stmt.execute(params![path, kind, depth])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A queue row's state, or `None` when the path isn't queued. A diagnostics/test
    /// probe — the drain loops use the atomic claim in
    /// [`next_queue_item`](Self::next_queue_item), never this.
    pub fn queue_state(&self, path: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT state FROM summary_queue WHERE path = ?1",
                params![path],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Queue statistics for status display.
    ///
    /// `pending`/`in_flight` count only rows backed by a live `entries` row — the real,
    /// processable backlog. Pending/in-flight rows whose path is NOT an entry (build
    /// artifacts later skipped, deleted files) are reported separately as `stale` so the
    /// headline backlog isn't inflated by un-processable rows (the drain self-cleans them
    /// and `indexa prune` removes them). For an entry-less index (the legitimate
    /// `deep`/`summarize`-without-`scan` workflow) every row counts as real work.
    pub fn queue_stats(&self) -> Result<QueueStats> {
        let has_entries: bool =
            self.conn
                .query_row("SELECT EXISTS(SELECT 1 FROM entries)", [], |r| r.get(0))?;
        // `ie` = "is a live entry" (1/0). With no entries, treat every row as live so the
        // entry-optional workflow is unaffected.
        let ie_expr = if has_entries {
            "(path IN (SELECT path FROM entries))"
        } else {
            "1"
        };
        let sql = format!(
            "SELECT
                COALESCE(SUM(CASE WHEN state='pending'   AND ie=1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='in_flight' AND ie=1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='done'      AND ie=1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='failed'    AND ie=1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state IN ('pending','in_flight') AND ie=0 THEN 1 ELSE 0 END), 0)
             FROM (SELECT state, {ie_expr} AS ie FROM summary_queue)"
        );
        let (pending, in_flight, done, failed, stale) = self.conn.query_row(&sql, [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(QueueStats {
            pending,
            in_flight,
            done,
            failed,
            stale,
        })
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
