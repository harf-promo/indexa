//! Schema DDL and lightweight migrations.

use super::Store;
use anyhow::Result;

/// Current schema version, stored in `PRAGMA user_version`. An open whose DB is already stamped at
/// this value skips the (idempotent but not free) DDL + migration probes in [`Store::init_schema`].
///
/// **INVARIANT: bump this whenever the DDL or any migration in `init_schema` changes** — otherwise a
/// DB stamped at the old value would skip the new migration and silently miss a column/table.
pub(super) const SCHEMA_VERSION: i64 = 2;

/// Does the `chunks` table's DDL declare AUTOINCREMENT? `true` when the table is absent
/// (a fresh DB — the CREATE below already includes it). Used to gate the one-time migration.
fn chunks_has_autoincrement(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT sql LIKE '%AUTOINCREMENT%' FROM sqlite_master
           WHERE type='table' AND name='chunks'",
        [],
        |r| r.get::<_, bool>(0),
    )
    .unwrap_or(true)
}

/// Does the `edges` table's CHECK constraint already allow `'calls'`?
/// Returns `true` when the table is absent (fresh DB) — DDL below already includes it.
fn edges_allows_calls(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT sql LIKE '%''calls''%' FROM sqlite_master WHERE type='table' AND name='edges'",
        [],
        |r| r.get::<_, bool>(0),
    )
    .unwrap_or(true)
}

impl Store {
    pub(super) fn init_schema(&mut self) -> Result<()> {
        // Connection-level PRAGMAs — these are per-connection (not persisted, except journal_mode
        // which is a DB-header setting), so they MUST run on every open regardless of schema
        // version. Cheap; kept out of the version-gated block below.
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            -- Auto-checkpoint the WAL every 1000 pages (~4 MB at 4 KB/page).
            -- Without this the WAL grows unboundedly when long-lived connections
            -- hold read locks that block automatic checkpointing.
            PRAGMA wal_autocheckpoint = 1000;
            -- WAL allows one writer at a time across connections. The worker pool, the
            -- per-event watcher, and the web summarize path each open their own connection,
            -- so without a busy timeout a contended write fails immediately with SQLITE_BUSY.
            -- Block-and-retry for up to 5s instead.
            PRAGMA busy_timeout = 5000;
            ",
        )?;

        // Fast path: a DB already stamped at the current version skips the idempotent-but-not-free
        // DDL (~20 `CREATE … IF NOT EXISTS`) and the ~10 `pragma_table_info`/`sqlite_master`
        // migration probes below. This runs on EVERY `Store::open` — MCP opens a fresh Store per
        // tool call and qa per ask — so the probes were pure repeated cost. Any mismatch (a fresh
        // DB reads 0, or an older/newer stamp) runs the full idempotent init and re-stamps, so old
        // DBs still migrate. See [`SCHEMA_VERSION`]'s bump invariant.
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if version == SCHEMA_VERSION {
            return Ok(());
        }

        self.conn.execute_batch(
            "
            -- Surface-scan entries (paths, sizes, surface hints)
            CREATE TABLE IF NOT EXISTS entries (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                parent_path TEXT,
                kind        TEXT NOT NULL CHECK(kind IN ('file','dir')),
                size        INTEGER NOT NULL DEFAULT 0,
                modified_s  INTEGER,
                hint_label  TEXT,
                hint_cat    TEXT,
                deep_policy TEXT,
                indexed_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                -- first_indexed_at (v0.10): original discovery time, never reset on rescan.
                -- In the base DDL so fresh DBs skip the ALTER migration below — and so the
                -- concurrent-open path never races on adding the column.
                first_indexed_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_path);
            CREATE INDEX IF NOT EXISTS idx_entries_kind   ON entries(kind);
            CREATE INDEX IF NOT EXISTS idx_entries_cat    ON entries(hint_cat);

            -- Deep-scan chunks (text + embeddings).
            -- AUTOINCREMENT (not a bare rowid) so ids are never reused after a re-deep
            -- deletes+reinserts a file's chunks. Stable ids are load-bearing for the ANN
            -- index (store::ann), which maps a node back to a chunk by id: a reused id would
            -- silently attribute the wrong file's content.
            CREATE TABLE IF NOT EXISTS chunks (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_path  TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                heading     TEXT NOT NULL DEFAULT '',
                text        TEXT NOT NULL,
                language    TEXT,
                embedding   BLOB,              -- IEEE-754 f32 little-endian bytes
                embed_model TEXT,
                indexed_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE (entry_path, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(entry_path);

            -- FTS5 full-text search over chunk text (standalone, populated manually)
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                text,
                heading,
                entry_path,
                chunk_id
            );

            -- Hierarchical summaries (one row per file or directory)
            CREATE TABLE IF NOT EXISTS summaries (
                path          TEXT PRIMARY KEY,
                kind          TEXT NOT NULL CHECK(kind IN ('file','dir')),
                parent_path   TEXT,
                depth         INTEGER NOT NULL DEFAULT 0,
                summary       TEXT NOT NULL,
                summary_l0    TEXT,
                embedding     BLOB,
                child_count   INTEGER NOT NULL DEFAULT 0,
                byte_size     INTEGER NOT NULL DEFAULT 0,
                model         TEXT NOT NULL DEFAULT '',
                source_hash   TEXT NOT NULL DEFAULT '',
                generated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                -- Provenance (v0.21): HOW this summary was produced, not just by which
                -- model. provider = adapter name ('ollama', 'claude-code', …);
                -- passes = refinement passes actually run; fallback = 1 when a lighter
                -- model was auto-substituted for the configured one. Substrate for the
                -- decision ledger (summary-drift questions need to know the lineage).
                provider      TEXT,
                passes        INTEGER,
                fallback      INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_summaries_parent ON summaries(parent_path);
            CREATE INDEX IF NOT EXISTS idx_summaries_depth  ON summaries(depth);
            CREATE INDEX IF NOT EXISTS idx_summaries_kind   ON summaries(kind);

            -- Background summarization queue
            CREATE TABLE IF NOT EXISTS summary_queue (
                path        TEXT PRIMARY KEY,
                kind        TEXT NOT NULL CHECK(kind IN ('file','dir')),
                depth       INTEGER NOT NULL DEFAULT 0,
                state       TEXT NOT NULL DEFAULT 'pending'
                                 CHECK(state IN ('pending','in_flight','done','failed')),
                attempts    INTEGER NOT NULL DEFAULT 0,
                enqueued_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                error       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_summary_queue_state ON summary_queue(state);

            -- Smart (semantic) classification — a SECOND axis over the technical
            -- hint_cat. One row per classified path (directories for now). Kept off
            -- the `entries` table on purpose: entries uses ON CONFLICT DO UPDATE which
            -- preserves row identity, but classifications hold user decisions that must
            -- never be automatically overwritten by a rescan. `source` distinguishes
            -- an auto suggestion from a user decision; 'ignored' is a sticky tombstone so
            -- a dismissed suggestion is not re-proposed on the next classify run.
            CREATE TABLE IF NOT EXISTS classifications (
                path         TEXT PRIMARY KEY,
                kind         TEXT NOT NULL CHECK(kind IN ('file','dir')),
                category     TEXT NOT NULL,
                confidence   REAL NOT NULL DEFAULT 1.0,
                source       TEXT NOT NULL DEFAULT 'auto'
                                  CHECK(source IN ('auto','user','ignored')),
                confirmed_at INTEGER,
                created_at   INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_classifications_source   ON classifications(source);
            CREATE INDEX IF NOT EXISTS idx_classifications_category ON classifications(category);

            -- Application/structure recognition (v0.66): which known software/stack/structure
            -- a DIRECTORY is (Rust crate, Next.js app, macOS .app bundle, Terraform module, …),
            -- derived by matching file-pattern signatures (see core::fingerprint). Multiple rows
            -- per dir are allowed (e.g. Node AND Next.js); is_primary flags the most-specific
            -- winner. Unlike classifications, these carry NO user decision — they are fully
            -- machine-derived and re-derivable from the tree, so they ARE cleared by the
            -- entries.rs delete paths and the prune orphan sweep (the classifications lifecycle,
            -- not the decisions/weights one).
            CREATE TABLE IF NOT EXISTS directory_apps (
                path         TEXT NOT NULL,
                app_kind     TEXT NOT NULL,
                app_name     TEXT NOT NULL,
                family       TEXT NOT NULL,
                specificity  INTEGER NOT NULL DEFAULT 10,
                is_primary   INTEGER NOT NULL DEFAULT 0,
                markers_json TEXT NOT NULL DEFAULT '[]',
                source       TEXT NOT NULL DEFAULT 'builtin',
                detected_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (path, app_kind)
            );
            CREATE INDEX IF NOT EXISTS idx_dirapps_path    ON directory_apps(path);
            CREATE INDEX IF NOT EXISTS idx_dirapps_primary ON directory_apps(path) WHERE is_primary = 1;
            CREATE INDEX IF NOT EXISTS idx_dirapps_kind    ON directory_apps(app_kind);

            -- Code-relationship graph (D1 + D2). One row per edge from a code file:
            --   kind='imports' → to_ref is an imported module/path
            --   kind='defines' → to_ref is a symbol defined in the file
            --   kind='calls'   → to_ref is a function/method name called by the file (D2)
            -- Composite PK dedups identical edges; idx_edges_to powers reverse lookups
            -- (who imports X / who defines Y / who calls Z). Re-deep of a file replaces
            -- its rows (delete-by-from_path then insert), mirroring chunks.
            CREATE TABLE IF NOT EXISTS edges (
                from_path TEXT NOT NULL,
                kind      TEXT NOT NULL CHECK(kind IN ('imports','defines','calls')),
                to_ref    TEXT NOT NULL,
                PRIMARY KEY (from_path, kind, to_ref)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_edges_to   ON edges(kind, to_ref);
            CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_path);

            -- Context Packs (v0.9): named, cross-directory context bundles.
            -- A pack is a user-curated set of paths that form a coherent topic
            -- (e.g. 'Auth', 'Tax 2025', 'Client X'). Paths may span multiple
            -- roots. The pack can be exported as a single XML/MD/JSON file.
            CREATE TABLE IF NOT EXISTS packs (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                description TEXT,
                created_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE TABLE IF NOT EXISTS pack_paths (
                pack_id  TEXT NOT NULL REFERENCES packs(id) ON DELETE CASCADE,
                path     TEXT NOT NULL,
                added_at INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (pack_id, path)
            );
            -- Importance weights (v0.8): user-controlled boosts per file, directory,
            -- or classification category. A weight > 1.0 promotes the target in search
            -- results; 0.0 effectively silences it. 'auto' rows are recency-based
            -- suggestions that are never overwritten by the user.
            CREATE TABLE IF NOT EXISTS importance_weights (
                target_kind TEXT NOT NULL CHECK(target_kind IN ('file','dir','category')),
                target      TEXT NOT NULL,
                weight      REAL NOT NULL DEFAULT 1.0 CHECK(weight >= 0.0),
                source      TEXT NOT NULL DEFAULT 'user' CHECK(source IN ('user','auto')),
                reason      TEXT,
                updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (target_kind, target)
            );
            CREATE INDEX IF NOT EXISTS idx_weights_kind ON importance_weights(target_kind);

            -- Saved searches: named, reusable `ask` queries (question + retrieval mode + scope).
            CREATE TABLE IF NOT EXISTS saved_queries (
                name       TEXT PRIMARY KEY,
                question   TEXT NOT NULL,
                mode       TEXT NOT NULL DEFAULT 'rrf',
                scope      TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            -- Conversational Ask (v0.64): a session groups ordered Q&A turns so a
            -- follow-up question can be resolved against, and answered with, the prior
            -- turns. Both tables are brand-new, so the bare CREATE TABLE IF NOT EXISTS
            -- here is the whole migration (no ALTER / IMMEDIATE-tx guard needed — that
            -- pattern is only for column-adds / table-recreates that race). Like
            -- importance_weights / decisions, a conversation is standing user state: it
            -- SURVIVES entry removal and is NOT cleared by the entries.rs delete paths.
            CREATE TABLE IF NOT EXISTS ask_sessions (
                id         TEXT PRIMARY KEY,           -- client- or server-generated opaque id
                scope      TEXT,                       -- the scope the session was opened with
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE TABLE IF NOT EXISTS conversation_turns (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id   TEXT NOT NULL REFERENCES ask_sessions(id) ON DELETE CASCADE,
                turn_index   INTEGER NOT NULL,         -- 0-based, monotonic within a session
                question     TEXT NOT NULL,
                answer       TEXT NOT NULL,
                sources_json TEXT NOT NULL DEFAULT '[]', -- serialized [{path,heading,snippet}]
                created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE (session_id, turn_index)
            );
            CREATE INDEX IF NOT EXISTS idx_turns_session
                ON conversation_turns(session_id, turn_index);

            -- Token-savings telemetry (v0.23): one row per retrieval call, across every
            -- surface ('mcp' | 'web' | 'cli'). surface deliberately has NO CHECK — widening
            -- the edges.kind CHECK cost a table-recreate migration above; valid values live
            -- in Rust. bytes_counterfactual = full on-disk size of every file behind what
            -- was served — the honest definition is documented in store::usage. Growth is
            -- capped by gc_usage(), called opportunistically from record_tool_usage.
            CREATE TABLE IF NOT EXISTS tool_usage (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                surface              TEXT NOT NULL,
                tool                 TEXT NOT NULL,
                bytes_served         INTEGER NOT NULL DEFAULT 0,
                bytes_counterfactual INTEGER NOT NULL DEFAULT 0,
                at                   INTEGER NOT NULL DEFAULT (unixepoch()),
                -- Conversational-Ask session this call belonged to (NULL for stateless
                -- calls). Lets a session show its own cumulative savings; see store::usage.
                session_id           TEXT,
                -- What `bytes_served` measured for this row: surfaces disagree (MCP records
                -- the full rendered tool response; web/CLI `ask` record answer+citations).
                -- NULL/'' = unspecified (legacy rows / untagged callers). Values are the
                -- BASIS_* constants in indexa_query::impact; kept CHECK-free (Rust owns them,
                -- same rule as `surface`). Lets the blended ledger reconcile per-surface.
                served_basis         TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_tool_usage_at ON tool_usage(at);
            -- NOTE: the idx_tool_usage_session index is intentionally NOT created here. On a
            -- pre-v0.69 index the CREATE TABLE above is a no-op (the table already exists without a
            -- session_id column), so creating an index on session_id in this base block would fail
            -- (no such column) before the ALTER migration below adds it. The migration creates the
            -- index idempotently AFTER ensuring the column exists -- fresh and upgraded DBs alike.

            -- Decision Ledger (v0.22): every uncertain judgment call becomes a row —
            -- one row = one question + its answer. The row fills in place on answer;
            -- the ONLY in-place lifecycle transition is open → decided/dismissed/expired.
            -- Changing or re-asking APPENDS a new row chained via parent_id, and the
            -- prior row gets superseded_by stamped (the only post-decision mutation).
            -- Current state = decided rows WHERE superseded_by IS NULL. decision_type
            -- deliberately has NO CHECK constraint — widening the edges.kind CHECK cost
            -- a table-recreate migration above; type validation lives in Rust.
            -- Decisions SURVIVE entry removal, like importance_weights (a recorded
            -- answer is standing user intent that outlives the entries row): they are
            -- NOT cleared by the entries.rs delete paths — vanished subjects are
            -- expired by the sweep, recorded, never silently dropped.
            CREATE TABLE IF NOT EXISTS decisions (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                decision_type TEXT NOT NULL,
                subject       TEXT NOT NULL,               -- stable key: path, cluster key, symbol
                params        TEXT NOT NULL DEFAULT '{}',  -- JSON evidence/template params
                options       TEXT NOT NULL DEFAULT '[]',  -- JSON array of candidate answers
                auto_value    TEXT,
                chosen        TEXT,
                source        TEXT CHECK(source IN ('auto','user','system')),
                confidence    REAL,
                evidence_hash TEXT NOT NULL DEFAULT '',    -- re-ask fingerprint
                priority      INTEGER NOT NULL DEFAULT 50,
                status        TEXT NOT NULL DEFAULT 'open'
                                   CHECK(status IN ('open','decided','dismissed','expired')),
                parent_id     INTEGER REFERENCES decisions(id),     -- revision chain
                superseded_by INTEGER REFERENCES decisions(id),
                effects             TEXT,     -- applied projection (JSON); NULL ⇒ repair-sweep target
                effects_applied_at  INTEGER,
                created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
                decided_at    INTEGER
            );
            -- At most one OPEN question per (type, subject); record_decision races
            -- resolve via ON CONFLICT DO NOTHING against this partial index.
            CREATE UNIQUE INDEX IF NOT EXISTS idx_decisions_open_unique
                ON decisions(decision_type, subject) WHERE status='open';
            CREATE INDEX IF NOT EXISTS idx_decisions_inbox
                ON decisions(priority DESC, created_at DESC) WHERE status='open';
            CREATE INDEX IF NOT EXISTS idx_decisions_key ON decisions(decision_type, subject);

            -- Every path a decision touches (cluster members, not just the subject) —
            -- powers the does-an-existing-question-already-cover-this-file lookup.
            CREATE TABLE IF NOT EXISTS decision_paths (
                decision_id INTEGER NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
                path        TEXT NOT NULL,
                PRIMARY KEY (decision_id, path)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_decision_paths_path ON decision_paths(path);

            -- Insights (v0.10): first_indexed_at is populated separately via migration.
            ",
        )?;

        // Migration: add entries.first_indexed_at (v0.10) — the original discovery
        // timestamp. Unlike indexed_at (reset on every rescan), this is set once and
        // never overwritten, enabling "what was added this week" queries.
        //
        // Note: for databases that predate this column, the backfill below seeds
        // first_indexed_at from indexed_at (the last-rescan time), which may be recent.
        // So the FIRST weekly-diff after upgrading an old DB can over-report files as
        // "added this week." This self-corrects: every subsequent insert stamps a true
        // discovery time. Fresh DBs get the column from the base DDL and are unaffected.
        let has_first_indexed_at: bool = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('entries') WHERE name = 'first_indexed_at'")?
            .exists([])?;
        if !has_first_indexed_at {
            self.conn.execute_batch(
                "ALTER TABLE entries ADD COLUMN first_indexed_at INTEGER;
                 UPDATE entries SET first_indexed_at = indexed_at WHERE first_indexed_at IS NULL;",
            )?;
        }

        // Migration: add summaries.summary_l0 (L0 one-line abstract) to databases
        // created before tiered summaries existed. SQLite has no ADD COLUMN IF NOT
        // EXISTS, so we check table_info first and ignore if already present.
        let has_l0: bool = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('summaries') WHERE name = 'summary_l0'")?
            .exists([])?;
        if !has_l0 {
            self.conn
                .execute_batch("ALTER TABLE summaries ADD COLUMN summary_l0 TEXT;")?;
        }

        // Migration: summaries provenance columns (v0.21) — provider / passes / fallback.
        // Checked per column (not as a trio) so a DB that somehow has a subset still
        // converges; each ALTER is independently idempotent.
        for (col, ddl) in [
            (
                "provider",
                "ALTER TABLE summaries ADD COLUMN provider TEXT;",
            ),
            ("passes", "ALTER TABLE summaries ADD COLUMN passes INTEGER;"),
            (
                "fallback",
                "ALTER TABLE summaries ADD COLUMN fallback INTEGER;",
            ),
        ] {
            let present: bool = self
                .conn
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('summaries') WHERE name = '{col}'"
                ))?
                .exists([])?;
            if !present {
                self.conn.execute_batch(ddl)?;
            }
        }

        // Migration: tool_usage.session_id (per-session savings ledger). Fresh DBs get it
        // from the base DDL; older DBs add it here. Nullable — stateless (non-session) calls
        // leave it NULL, so existing rows and non-ask tools are unaffected.
        let has_usage_session_id: bool = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('tool_usage') WHERE name = 'session_id'")?
            .exists([])?;
        if !has_usage_session_id {
            self.conn
                .execute_batch("ALTER TABLE tool_usage ADD COLUMN session_id TEXT;")?;
        }
        // Create the session index HERE (not in the base DDL) so it runs only after the column is
        // guaranteed to exist — on a fresh DB (column from the base CREATE TABLE) and on an
        // upgraded DB (column just added by the ALTER above). Idempotent, so it's a no-op once
        // present. Putting it in the base DDL broke opening any pre-v0.69 index with
        // "no such column: session_id" (the CREATE TABLE IF NOT EXISTS skipped the new column).
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_tool_usage_session ON tool_usage(session_id);",
        )?;

        // Migration: tool_usage.served_basis (G8 — tags what `bytes_served` measured so the
        // blended savings ledger can reconcile per-surface). Fresh DBs get it from the base
        // DDL; older DBs add it here. Nullable — pre-migration rows and untagged callers stay
        // NULL/'', reported as "unspecified".
        let has_served_basis: bool = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('tool_usage') WHERE name = 'served_basis'")?
            .exists([])?;
        if !has_served_basis {
            self.conn
                .execute_batch("ALTER TABLE tool_usage ADD COLUMN served_basis TEXT;")?;
        }

        // Migration: give `chunks.id` AUTOINCREMENT on databases created before stable ids
        // (a bare rowid is reused after a delete, which would mis-attribute ANN results to a
        // different chunk). SQLite can't ALTER a column to add AUTOINCREMENT, so recreate the
        // table preserving every id (so the FTS chunk_id references stay valid) and let
        // sqlite_sequence continue from MAX(id). One-time O(rows) copy on first open.
        //
        // `worker` and `serve` are separate processes on one DB, so two could open a legacy
        // DB at once. The non-atomic CREATE/DROP/RENAME would then race (a "table exists"
        // error, or a window where `chunks` is gone). Guard with a fast lock-free pre-check,
        // then run the whole migration inside one IMMEDIATE transaction with the check
        // re-done under the write lock: the second opener blocks, re-checks, sees
        // AUTOINCREMENT, and skips. (busy_timeout, set above, bounds the wait.)
        if !chunks_has_autoincrement(&self.conn) {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !chunks_has_autoincrement(&tx) {
                tx.execute_batch(
                    "
                    CREATE TABLE chunks_migrate (
                        id          INTEGER PRIMARY KEY AUTOINCREMENT,
                        entry_path  TEXT NOT NULL,
                        seq         INTEGER NOT NULL,
                        heading     TEXT NOT NULL DEFAULT '',
                        text        TEXT NOT NULL,
                        language    TEXT,
                        embedding   BLOB,
                        embed_model TEXT,
                        indexed_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                        UNIQUE (entry_path, seq)
                    );
                    INSERT INTO chunks_migrate
                        (id, entry_path, seq, heading, text, language, embedding, embed_model, indexed_at)
                        SELECT id, entry_path, seq, heading, text, language, embedding, embed_model, indexed_at
                        FROM chunks;
                    DROP TABLE chunks;
                    ALTER TABLE chunks_migrate RENAME TO chunks;
                    CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(entry_path);
                    ",
                )?;
            }
            tx.commit()?;
        }

        // Migration: widen the edges.kind CHECK to include 'calls' (D2 call edges).
        // SQLite can't ALTER a constraint, so recreate the table when the old DDL is
        // detected. Same IMMEDIATE-tx double-check pattern as the chunks migration above.
        if !edges_allows_calls(&self.conn) {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !edges_allows_calls(&tx) {
                tx.execute_batch(
                    "
                    CREATE TABLE edges_new (
                        from_path TEXT NOT NULL,
                        kind      TEXT NOT NULL CHECK(kind IN ('imports','defines','calls')),
                        to_ref    TEXT NOT NULL,
                        PRIMARY KEY (from_path, kind, to_ref)
                    ) WITHOUT ROWID;
                    INSERT OR IGNORE INTO edges_new SELECT * FROM edges;
                    DROP TABLE edges;
                    ALTER TABLE edges_new RENAME TO edges;
                    CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(kind, to_ref);
                    ",
                )?;
            }
            tx.commit()?;
        }

        // Migration: add chunks.content_hash (v0.42) — a SHA-256 hex digest of the raw
        // chunk text, used as a cache key to skip re-embedding chunks whose text is unchanged.
        // Nullable so existing rows (NULL) are treated as "no cache" and re-embedded normally.
        // The companion index makes the per-file hash lookup fast.
        //
        // Guard with an IMMEDIATE transaction (same pattern as the AUTOINCREMENT migration):
        // the fast pre-check avoids the write-lock on the common "already migrated" path, and
        // the re-check inside the exclusive lock prevents "duplicate column" races when
        // multiple processes open the DB concurrently on a legacy database.
        let needs_content_hash = |conn: &rusqlite::Connection| -> rusqlite::Result<bool> {
            conn.prepare("SELECT 1 FROM pragma_table_info('chunks') WHERE name = 'content_hash'")?
                .exists([])
                .map(|present| !present)
        };
        if needs_content_hash(&self.conn)? {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if needs_content_hash(&tx)? {
                tx.execute_batch(
                    "ALTER TABLE chunks ADD COLUMN content_hash TEXT;
                     CREATE INDEX IF NOT EXISTS idx_chunks_content_hash
                         ON chunks(entry_path, content_hash);",
                )?;
            }
            tx.commit()?;
        }

        // Referential integrity for chunks/summaries/edges → entries is maintained by
        // MANUAL multi-table cleanup in entries.rs (delete_entry / delete_subtree /
        // delete_path_artifacts_exact), NOT by a database FK. There is deliberately no
        // `REFERENCES entries(path) ON DELETE CASCADE` on those tables because Indexa's
        // data model intentionally allows chunks/summaries to exist with no `entries`
        // row: `indexa deep <file>` and `indexa summarize <path>` run without a prior
        // `scan`, and `chunks_current_for_mtime` is documented to hold "for a file with
        // no entries row". A strict FK would reject those legitimate writes.
        //
        // The integrity contract (every entry-delete path clears all child rows) is
        // locked by `delete_entry_leaves_no_orphans` / `delete_subtree_leaves_no_orphans`
        // in store::tests — add any new entry-keyed child table to both the cleanup
        // statements and those tests. `directory_apps` (v0.66) follows this contract:
        // machine-derived, re-derivable, so it is cleared on delete + orphan-pruned.
        // (`importance_weights` is intentionally exempt:
        // weights persist across entry removal by design — see store::weights.
        // `decisions`/`decision_paths` are exempt for the same reason: a recorded
        // answer is standing user intent; vanished subjects are expired by the sweep,
        // not deleted — see store::decisions. `ask_sessions`/`conversation_turns` are
        // exempt too: a conversation is standing user state, not entry-keyed — see
        // store::sessions.)

        // Stamp the schema version LAST — only after all DDL + migrations succeeded — so a future
        // open can take the fast path above. A failure before here leaves user_version unchanged,
        // so the next open retries the full init.
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }
}
