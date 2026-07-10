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

// Delegate to the shared implementation in `indexa_core::text` so the formula
// stays a single source of truth across the savings line, impact readout, and JS.
fn human_count(n: u64) -> String {
    crate::text::human_count(n)
}

impl Store {
    /// Record one retrieval call, untagged. `surface` is 'mcp' | 'web' | 'cli'.
    /// Delegates to [`record_tool_usage_with_basis`] with an empty `served_basis`
    /// (kept for callers that don't distinguish a serving basis); the row reads
    /// back as "unspecified" in [`usage_by_basis`].
    pub fn record_tool_usage(
        &mut self,
        surface: &str,
        tool: &str,
        bytes_served: u64,
        bytes_counterfactual: u64,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.record_tool_usage_with_basis(
            surface,
            tool,
            bytes_served,
            bytes_counterfactual,
            session_id,
            "",
        )
    }

    /// Record one retrieval call, tagging what `bytes_served` measured (`served_basis`).
    /// Surfaces disagree — MCP records the full rendered tool response, web/CLI `ask`
    /// record answer+citations — so an untagged blended ledger can't reconcile; the tag
    /// (a `BASIS_*` constant from `indexa_query::impact`, `""` = unspecified) lets
    /// [`usage_by_basis`] split the aggregate. Every ~1000th insert opportunistically
    /// GCs rows past the retention window so the table can't grow unbounded.
    pub fn record_tool_usage_with_basis(
        &mut self,
        surface: &str,
        tool: &str,
        bytes_served: u64,
        bytes_counterfactual: u64,
        session_id: Option<&str>,
        served_basis: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_usage
                 (surface, tool, bytes_served, bytes_counterfactual, session_id, served_basis)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            // i64 at the SQL boundary: rusqlite has no u64 ToSql/FromSql.
            params![
                surface,
                tool,
                bytes_served as i64,
                bytes_counterfactual as i64,
                session_id,
                served_basis
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

    /// Per-`served_basis` aggregate over the last `since_secs` seconds, most-saving first.
    /// The savings ledger blends bases (MCP records the full rendered response; web/CLI `ask`
    /// record answer+citations), so the single [`usage_summary`] figure mixes them; this splits
    /// it so `status` / `get_stats` can reconcile per-surface. `NULL`/empty tags (legacy rows,
    /// untagged callers) group under `"unspecified"`.
    pub fn usage_by_basis(&self, since_secs: i64) -> Result<Vec<(String, UsageSummary)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(NULLIF(served_basis, ''), 'unspecified') AS basis,
                    COUNT(*),
                    COALESCE(SUM(bytes_served), 0),
                    COALESCE(SUM(bytes_counterfactual), 0)
               FROM tool_usage WHERE at >= unixepoch() - ?1
              GROUP BY basis
              ORDER BY COALESCE(SUM(bytes_counterfactual), 0) - COALESCE(SUM(bytes_served), 0) DESC,
                       basis ASC",
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

    /// Cumulative usage for one Conversational-Ask session (all time, not windowed) —
    /// the per-session savings ledger. Sums only rows tagged with this `session_id`, so a
    /// session can show how much it has saved versus serving whole files.
    pub fn session_usage_summary(&self, session_id: &str) -> Result<UsageSummary> {
        let row = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(bytes_served), 0),
                    COALESCE(SUM(bytes_counterfactual), 0)
               FROM tool_usage WHERE session_id = ?1",
            params![session_id],
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

    /// Per-path counterfactual sizes for a set of served paths — the itemized form of
    /// [`Store::counterfactual_bytes_for_paths`] (which sums this). Each DISTINCT path appears
    /// once, in **first-seen order** (stable for display in the "show the math" breakdown).
    /// Size resolution is identical: `entries.size` first, then `summaries.byte_size` for
    /// directories (`NULLIF(…, 0)`), else 0 for unknown paths (under-counting stays honest).
    pub fn counterfactual_sizes_for_paths(&self, paths: &[&str]) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(
                        NULLIF((SELECT size FROM entries WHERE path = ?1), 0),
                        (SELECT byte_size FROM summaries WHERE path = ?1),
                        0)",
        )?;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<(String, u64)> = Vec::new();
        for &path in paths {
            if !seen.insert(path) {
                continue; // duplicate hit in one response — don't double-count
            }
            let size: i64 = stmt.query_row(params![path], |r| r.get(0))?;
            out.push((path.to_string(), size.max(0) as u64));
        }
        Ok(out)
    }

    /// Counterfactual bytes for a set of served paths: the sum of
    /// [`Store::counterfactual_sizes_for_paths`] (single source of truth for the size math).
    pub fn counterfactual_bytes_for_paths(&self, paths: &[&str]) -> Result<u64> {
        Ok(self
            .counterfactual_sizes_for_paths(paths)?
            .iter()
            .map(|(_, size)| size)
            .sum())
    }
}
