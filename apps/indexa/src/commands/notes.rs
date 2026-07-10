//! `indexa note add` — attach a Markdown note to a Context Pack. Closes the write-back loop that,
//! until now, only the MCP `add_note` tool exercised (`crates/mcp/src/admin.rs`): a good grounded
//! answer or idea can be captured straight back into the searchable context that produced it.

use anyhow::Result;
use indexa_core::{config::Config, notes::write_note_file, store::Store};

use super::cmd_deep;
use super::helpers::require_index_db;

/// Write the note file and register it as a pack member. Pure w.r.t. the index (no reindex, no
/// Ollama) — split out from `cmd_note_add_at` so it's hermetically testable on its own and so the
/// durable part of the flow (file written + pack membership recorded) is obviously independent of
/// the best-effort reindex that follows it.
fn write_and_register_note(
    db_path: &std::path::Path,
    data_dir: &std::path::Path,
    pack: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let mut store = Store::open(db_path)?;
    let pack_rec = store.pack_by_name(pack)?.ok_or_else(|| {
        anyhow::anyhow!("no pack named \"{pack}\" — create it first with `indexa pack create`")
    })?;
    let note_path = write_note_file(data_dir, pack, title, body)?;
    let note_path_str = note_path.to_string_lossy().into_owned();
    store.add_pack_paths(&pack_rec.id, std::slice::from_ref(&note_path_str))?;
    Ok(note_path_str)
}

/// `cmd_note_add` with the DB path injected, so it's hermetically testable — mirrors
/// `cmd_pack_refresh_at`'s testability pattern.
pub(crate) async fn cmd_note_add_at(
    db_path: &std::path::Path,
    pack: String,
    title: String,
    body: String,
    cfg: &Config,
) -> Result<()> {
    let data_dir = db_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("index db path has no parent directory"))?;
    let note_path = write_and_register_note(db_path, data_dir, &pack, &title, &body)?;
    println!("Note \"{title}\" added to pack \"{pack}\".");
    println!("  File: {note_path}");

    // Best-effort reindex: the note is already durably saved and registered above, so a down
    // Ollama (or any other deep-index failure) must not fail the whole command — the user can
    // pick it up later with `indexa pack refresh`. Embed-only, in-process (no subprocess needed —
    // `cmd_deep` treats each path as its own root and `walk()` already handles a bare file root),
    // mirroring `cmd_pack_refresh_at`'s reindex call.
    match cmd_deep(
        vec![note_path],
        None,
        false,
        "augment".to_string(),
        false,
        false,
        false,
        cfg,
    )
    .await
    {
        Ok(()) => println!("  Indexed."),
        Err(e) => println!(
            "  ⚠ Indexing the note failed ({e:#}) — run `indexa pack refresh \"{pack}\"` once the \
             embedder is reachable."
        ),
    }
    Ok(())
}

pub(crate) async fn cmd_note_add(
    pack: String,
    title: String,
    body: String,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    cmd_note_add_at(&db_path, pack, title, body, cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_register_note_creates_file_and_pack_membership() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store.create_pack("MyPack", None).unwrap();
        drop(store);

        let note_path = write_and_register_note(
            &db_path,
            dir.path(),
            "MyPack",
            "Reset flow",
            "Users reset via a signed email link.",
        )
        .unwrap();

        assert!(std::path::Path::new(&note_path).exists());
        let content = std::fs::read_to_string(&note_path).unwrap();
        assert!(content.contains("# Reset flow"));
        assert!(content.contains("Users reset via a signed email link."));

        let store = Store::open(&db_path).unwrap();
        let pack = store.pack_by_name("MyPack").unwrap().unwrap();
        assert_eq!(pack.path_count, 1);
        let paths = store.pack_paths(&pack.id).unwrap();
        assert_eq!(paths, vec![note_path]);
    }

    #[test]
    fn write_and_register_note_rejects_unknown_pack() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idx.db");
        // No pack created — must error, not silently write an orphan note.
        let err = write_and_register_note(&db_path, dir.path(), "Ghost", "T", "B").unwrap_err();
        assert!(err.to_string().contains("no pack named \"Ghost\""));
    }

    #[tokio::test]
    async fn cmd_note_add_at_saves_even_when_reindex_is_unreachable() {
        // No Ollama running in CI: `cmd_deep`'s preflight will fail, but the note must still be
        // durably saved and registered — the whole point of making the reindex best-effort.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("idx.db");
        let mut store = Store::open(&db_path).unwrap();
        store.create_pack("MyPack", None).unwrap();
        drop(store);

        let cfg = Config::default();
        cmd_note_add_at(
            &db_path,
            "MyPack".to_string(),
            "Idea".to_string(),
            "Some body text.".to_string(),
            &cfg,
        )
        .await
        .unwrap();

        let store = Store::open(&db_path).unwrap();
        let pack = store.pack_by_name("MyPack").unwrap().unwrap();
        assert_eq!(pack.path_count, 1);
    }
}
