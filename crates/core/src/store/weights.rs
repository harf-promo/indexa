//! Importance weights (v0.8): per-file, per-directory, or per-category boosts
//! that are applied multiplicatively to search-hit RRF scores.

use super::{Store, WeightRecord};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

impl Store {
    /// Insert or replace an importance weight.
    /// `target_kind` must be `"file"`, `"dir"`, or `"category"`.
    /// `weight` must be ≥ 0.0 (0 = silenced, 1 = neutral, >1 = boosted).
    pub fn set_weight(
        &mut self,
        target_kind: &str,
        target: &str,
        weight: f32,
        source: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO importance_weights (target_kind, target, weight, source, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
             ON CONFLICT(target_kind, target) DO UPDATE SET
                 weight     = excluded.weight,
                 source     = excluded.source,
                 reason     = excluded.reason,
                 updated_at = unixepoch()",
            params![target_kind, target, weight, source, reason],
        )?;
        Ok(())
    }

    /// Resolve the effective weight for `path` using longest-prefix matching:
    /// 1. Exact file match (`target_kind='file'`, `target=path`).
    /// 2. Nearest ancestor directory (`target_kind='dir'`, longest prefix of `path`).
    /// 3. Category weight from the `classifications` table if the path has a category.
    /// 4. Falls back to 1.0 (neutral) if nothing matches.
    pub fn weight_for(&self, path: &str) -> Result<f32> {
        // 1. Exact file match.
        if let Some(w) = self
            .conn
            .query_row(
                "SELECT weight FROM importance_weights WHERE target_kind='file' AND target=?1",
                params![path],
                |r| r.get::<_, f32>(0),
            )
            .optional()?
        {
            return Ok(w);
        }

        // 2. Longest ancestor directory match: iterate path ancestors from deepest to shallowest.
        let mut p = std::path::Path::new(path);
        while let Some(parent) = p.parent() {
            let parent_str = parent.to_string_lossy();
            if let Some(w) = self
                .conn
                .query_row(
                    "SELECT weight FROM importance_weights WHERE target_kind='dir' AND target=?1",
                    params![parent_str.as_ref()],
                    |r| r.get::<_, f32>(0),
                )
                .optional()?
            {
                return Ok(w);
            }
            p = parent;
        }

        // 3. Category weight via the classifications table.
        if let Some(category) = self
            .conn
            .query_row(
                "SELECT category FROM classifications WHERE path=?1 AND source != 'ignored'",
                params![path],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            if let Some(w) = self.conn.query_row(
                "SELECT weight FROM importance_weights WHERE target_kind='category' AND target=?1",
                params![category],
                |r| r.get::<_, f32>(0),
            ).optional()? {
                return Ok(w);
            }
        }

        Ok(1.0)
    }

    /// List all importance weights, optionally filtered by target_kind.
    pub fn list_weights(&self, kind_filter: Option<&str>) -> Result<Vec<WeightRecord>> {
        fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WeightRecord> {
            Ok(WeightRecord {
                target_kind: r.get(0)?,
                target: r.get(1)?,
                weight: r.get(2)?,
                source: r.get(3)?,
                reason: r.get(4)?,
                updated_at: r.get(5)?,
            })
        }
        if let Some(k) = kind_filter {
            let mut stmt = self.conn.prepare(
                "SELECT target_kind, target, weight, source, reason, updated_at
                 FROM importance_weights WHERE target_kind=?1 ORDER BY target_kind, target",
            )?;
            let rows = stmt.query_map(params![k], map_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT target_kind, target, weight, source, reason, updated_at
                 FROM importance_weights ORDER BY target_kind, target",
            )?;
            let rows = stmt.query_map([], map_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }
    }

    /// Delete an importance weight by exact (kind, target) key.
    pub fn delete_weight(&mut self, target_kind: &str, target: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM importance_weights WHERE target_kind=?1 AND target=?2",
            params![target_kind, target],
        )?;
        Ok(())
    }

    /// Suggest auto importance weights based on modification recency.
    /// Returns `(path, weight)` pairs ordered by recency (most recent first).
    /// Scale: modified in last 7 days → 2.0; 30 days → 1.5; 90 days → 1.2; older → 1.0.
    pub fn suggest_weights_by_recency(&self, threshold_days: i64) -> Result<Vec<(String, f32)>> {
        let cutoff = threshold_days * 86_400;
        let mut stmt = self.conn.prepare(
            "SELECT path, modified_s
             FROM entries
             WHERE modified_s IS NOT NULL
               AND modified_s > (unixepoch() - ?1)
             ORDER BY modified_s DESC
             LIMIT 200",
        )?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (path, mtime) = row?;
            let age_days = (now - mtime).max(0) / 86_400;
            let weight = if age_days <= 7 {
                2.0f32
            } else if age_days <= 30 {
                1.5
            } else if age_days <= 90 {
                1.2
            } else {
                1.0
            };
            result.push((path, weight));
        }
        Ok(result)
    }

    /// Apply importance weight boosts to a list of search hits (multiplicative on rrf_score).
    ///
    /// Pre-loads the whole `importance_weights` table into in-memory maps once (the table is
    /// small — user-curated) so resolving each hit needs no per-hit SQL for file/dir weights.
    /// Previously this fired up to `depth`-many ancestor queries per hit (≈200 round-trips for
    /// a 20-hit answer over an 8-deep tree). Category weights still consult `classifications`,
    /// but only when category weights exist, and the result is cached per path within the call.
    pub fn boost_with_weights(&self, hits: &mut [super::SearchHit]) -> Result<()> {
        use std::collections::HashMap;

        // Load the (small) weights table once into kind-specific maps.
        let mut file_w: HashMap<String, f32> = HashMap::new();
        let mut dir_w: HashMap<String, f32> = HashMap::new();
        let mut cat_w: HashMap<String, f32> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT target_kind, target, weight FROM importance_weights")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f32>(2)?,
                ))
            })?;
            for row in rows {
                let (kind, target, w) = row?;
                match kind.as_str() {
                    "file" => drop(file_w.insert(target, w)),
                    "dir" => drop(dir_w.insert(target, w)),
                    "category" => drop(cat_w.insert(target, w)),
                    _ => {}
                }
            }
        }
        // No weights set → nothing to do (the common case is now a single SELECT).
        if file_w.is_empty() && dir_w.is_empty() && cat_w.is_empty() {
            return Ok(());
        }

        // Cache classifications category lookups per path within this call.
        let mut cat_cache: HashMap<String, Option<String>> = HashMap::new();

        for hit in hits.iter_mut() {
            let path = &hit.entry_path;
            // 1. Exact file weight.
            let w = if let Some(&w) = file_w.get(path) {
                Some(w)
            } else {
                // 2. Nearest ancestor directory weight (longest prefix).
                let mut found = None;
                let mut p = std::path::Path::new(path);
                while let Some(parent) = p.parent() {
                    if let Some(&w) = dir_w.get(parent.to_string_lossy().as_ref()) {
                        found = Some(w);
                        break;
                    }
                    p = parent;
                }
                found
            };
            // 3. Category weight (only if category weights exist).
            let w = match w {
                Some(w) => w,
                None if !cat_w.is_empty() => {
                    let category = if let Some(c) = cat_cache.get(path) {
                        c.clone()
                    } else {
                        let c: Option<String> = self
                            .conn
                            .query_row(
                                "SELECT category FROM classifications \
                                 WHERE path=?1 AND source != 'ignored'",
                                params![path],
                                |r| r.get::<_, String>(0),
                            )
                            .optional()?;
                        cat_cache.insert(path.clone(), c.clone());
                        c
                    };
                    category.and_then(|c| cat_w.get(&c).copied()).unwrap_or(1.0)
                }
                None => 1.0,
            };
            if (w - 1.0f32).abs() > f32::EPSILON {
                hit.rrf_score *= w as f64;
            }
        }
        Ok(())
    }
}
