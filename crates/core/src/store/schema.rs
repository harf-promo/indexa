//! Schema DDL and lightweight migrations.

use super::Store;
use anyhow::Result;

impl Store {
    pub(super) fn init_schema(&self) -> Result<()> {
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
                indexed_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_path);
            CREATE INDEX IF NOT EXISTS idx_entries_kind   ON entries(kind);
            CREATE INDEX IF NOT EXISTS idx_entries_cat    ON entries(hint_cat);

            -- Deep-scan chunks (text + embeddings)
            CREATE TABLE IF NOT EXISTS chunks (
                id          INTEGER PRIMARY KEY,
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
            -- the `entries` table on purpose: `entries` is INSERT OR REPLACE'd on every
            -- rescan, which would wipe a user's confirmed labels. `source` distinguishes
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

            -- Code-relationship graph (D1). One row per edge from a code file:
            --   kind='imports' → to_ref is an imported module/path
            --   kind='defines' → to_ref is a symbol defined in the file
            -- Composite PK dedups identical edges; idx_edges_to powers reverse lookups
            -- (who imports module X / who defines symbol Y). Re-deep of a file replaces
            -- its rows (delete-by-from_path then insert), mirroring chunks.
            CREATE TABLE IF NOT EXISTS edges (
                from_path TEXT NOT NULL,
                kind      TEXT NOT NULL CHECK(kind IN ('imports','defines')),
                to_ref    TEXT NOT NULL,
                PRIMARY KEY (from_path, kind, to_ref)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(kind, to_ref);
            ",
        )?;

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

        Ok(())
    }
}
