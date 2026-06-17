//! Token-savings telemetry: measure the "saves your paid token budget" pitch
//! instead of asserting it. One `tool_usage` row per retrieval call; aggregated
//! into the savings line on `indexa status`, MCP `get_stats`, and `/api/stats`.
//!
//! ## What `bytes_counterfactual` means — the honest definition
//!
//! The bytes a client would have read WITHOUT the index: the full on-disk size
//! of every file behind what was served.
//!
//! - `get_summary` → the file's `entries.size` (fallback: `summaries.byte_size` —
//!   which for directories is the subtree total, since the entries row says 0).
//! - `search` / `ask` → SUM of DISTINCT entry sizes behind the returned hits.
//! - `read_file` → the file's full size (vs. the served, capped slice).
//!
//! It is an estimate of *avoided reading*, not a measured baseline: a real
//! client might have read fewer files, or more (re-reading across sessions).
//! Token numbers derived from it are always ≈ bytes/4 and labeled approximate.
//!
//! Recording is best-effort by contract: callers must never let a telemetry
//! failure fail the user's call (`let _ =` / `tracing::debug!` at the hook).

use super::Store;
use anyhow::Result;
use rusqlite::params;
use std::collections::HashSet;

/// The aggregation window every surface reports on ("this week").
pub const USAGE_WEEK_SECS: i64 = 7 * 24 * 60 * 60;

/// Rows older than this are garbage-collected; far beyond the reporting window
/// so the cap never bites a query, but bounds table growth on a busy index.
const USAGE_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;

/// Aggregate over a `tool_usage` window — see the module doc for what
/// `bytes_counterfactual` does (and does not) claim.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageSummary {
    pub calls: u64,
    pub bytes_served: u64,
    pub bytes_counterfactual: u64,
}

impl UsageSummary {
    /// The shared "tokens saved" sentence, or `None` when there is no usage to
    /// report. One implementation so `indexa status` and MCP `get_stats` say
    /// exactly the same (approximate) thing.
    pub fn savings_line(&self) -> Option<String> {
        if self.calls == 0 {
            return None;
        }
        let tokens = self.bytes_counterfactual.saturating_sub(self.bytes_served) / 4;
        // "bytes/token", not "chars/token": both quantities really are bytes
        // (UTF-8 lengths / on-disk sizes), and on multibyte-heavy text bytes
        // overstate chars — the label must not overstate the estimate.
        Some(format!(
            "This week Indexa served {} where whole-file context would have been {} — roughly {} tokens saved (estimated at ≈4 bytes/token).",
            human_size(self.bytes_served),
            human_size(self.bytes_counterfactual),
            human_count(tokens),
        ))
    }
}

fn human_size(bytes: u64) -> String {
    // Single source of truth in `crate::text` so this and the per-answer impact readout
    // (v0.59) format byte sizes identically.
    crate::text::human_bytes(bytes)
}

fn human_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

impl Store {
    /// Record one retrieval call. `surface` is 'mcp' | 'web' | 'cli' (no DB
    /// CHECK — see the schema comment). Every ~1000th insert opportunistically
    /// GCs rows past the retention window so the table can't grow unbounded.
    pub fn record_tool_usage(
        &mut self,
        surface: &str,
        tool: &str,
        bytes_served: u64,
        bytes_counterfactual: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_usage (surface, tool, bytes_served, bytes_counterfactual)
             VALUES (?1, ?2, ?3, ?4)",
            // i64 at the SQL boundary: rusqlite has no u64 ToSql/FromSql.
            params![
                surface,
                tool,
                bytes_served as i64,
                bytes_counterfactual as i64
            ],
        )?;
        if self.conn.last_insert_rowid() % 1000 == 0 {
            self.gc_usage(USAGE_RETENTION_SECS)?;
        }
        Ok(())
    }

    /// Per-tool aggregate over the last `since_secs` seconds, most-saving first.
    /// Powers the web "Impact" dashboard and `indexa status --json`'s `by_tool`
    /// breakdown — same `tool_usage` rows as [`usage_summary`], grouped by `tool`.
    /// Ordered by avoided bytes (counterfactual − served) descending so the
    /// highest-leverage tools surface at the top of the table.
    pub fn usage_by_tool(&self, since_secs: i64) -> Result<Vec<(String, UsageSummary)>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool,
                    COUNT(*),
                    COALESCE(SUM(bytes_served), 0),
                    COALESCE(SUM(bytes_counterfactual), 0)
               FROM tool_usage WHERE at >= unixepoch() - ?1
              GROUP BY tool
              ORDER BY COALESCE(SUM(bytes_counterfactual), 0) - COALESCE(SUM(bytes_served), 0) DESC,
                       tool ASC",
        )?;
        let rows = stmt
            .query_map(params![since_secs], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    UsageSummary {
                        calls: r.get::<_, i64>(1)? as u64,
                        bytes_served: r.get::<_, i64>(2)? as u64,
                        bytes_counterfactual: r.get::<_, i64>(3)? as u64,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Aggregate calls/bytes over the last `since_secs` seconds.
    pub fn usage_summary(&self, since_secs: i64) -> Result<UsageSummary> {
        let row = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(bytes_served), 0),
                    COALESCE(SUM(bytes_counterfactual), 0)
               FROM tool_usage WHERE at >= unixepoch() - ?1",
            params![since_secs],
            |r| {
                Ok(UsageSummary {
                    calls: r.get::<_, i64>(0)? as u64,
                    bytes_served: r.get::<_, i64>(1)? as u64,
                    bytes_counterfactual: r.get::<_, i64>(2)? as u64,
                })
            },
        )?;
        Ok(row)
    }

    /// Delete usage rows older than `older_than_secs`. Returns rows removed.
    pub fn gc_usage(&mut self, older_than_secs: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM tool_usage WHERE at < unixepoch() - ?1",
            params![older_than_secs],
        )?;
        Ok(n)
    }

    /// Counterfactual bytes for a set of served paths: the full on-disk size of
    /// each DISTINCT path (duplicate hits in one response don't double-count).
    /// `entries.size` first; `NULLIF(…, 0)` so directories (size 0 in entries)
    /// fall through to `summaries.byte_size`, the subtree total. Unknown paths
    /// contribute 0 — under-counting keeps the savings claim honest.
    pub fn counterfactual_bytes_for_paths(&self, paths: &[&str]) -> Result<u64> {
        let distinct: HashSet<&str> = paths.iter().copied().collect();
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(
                        NULLIF((SELECT size FROM entries WHERE path = ?1), 0),
                        (SELECT byte_size FROM summaries WHERE path = ?1),
                        0)",
        )?;
        let mut total: u64 = 0;
        for path in distinct {
            let size: i64 = stmt.query_row(params![path], |r| r.get(0))?;
            total += size.max(0) as u64;
        }
        Ok(total)
    }
}
