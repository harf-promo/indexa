//! Schema DDL and lightweight migrations.

use super::Store;
use anyhow::Result;

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
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            -- WAL allows one writer at a time across connections. The worker pool, the
            -- per-event watcher, and the web summarize path each open their own connection,
            -- so without a busy timeout a contended write fails immediately with SQLITE_BUSY.
            -- Block-and-retry for up to 5s instead.
            PRAGMA busy_timeout = 5000;

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
                generated_at  INTEGER NOT NULL DEFAULT (unixepoch())
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
        // statements and those tests. (`importance_weights` is intentionally exempt:
        // weights persist across entry removal by design — see store::weights.)

        Ok(())
    }
}
