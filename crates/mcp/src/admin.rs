//! Admin tools: `get_stats`, `prune`, `trigger_index`, and `add_note`.

use indexa_core::store::Store;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use crate::{mcp_err, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TriggerIndexParams {
    /// Absolute path to scan, deep-index, and summarize.
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddNoteParams {
    /// Name of an **existing** pack to attach the note to.
    /// Create the pack first with `create_pack` if it does not exist yet.
    pub pack: String,
    /// Short title for the note (becomes the Markdown `# heading` and the search slug).
    pub title: String,
    /// Markdown body of the note — the knowledge you want to persist and make searchable.
    pub body: String,
}

/// Absolute path to the running `indexa` binary. The MCP server IS `indexa mcp`, so `current_exe`
/// is the correct executable to re-invoke for `index`. Spawning the bare name `"indexa"` instead
/// runs whatever is first on `$PATH` — which for a GUI-launched MCP client (minimal environment)
/// may be missing entirely or a different, older install, contradicting the "doctor/status/MCP are
/// authoritative" contract this crate already enforces via `detect_skew`. Canonicalized so it
/// survives symlinked installs (mirrors `commands::mcp_install`).
fn indexa_exe() -> Result<std::path::PathBuf, ErrorData> {
    let exe = std::env::current_exe()
        .map_err(|e| mcp_err(format!("cannot resolve the running indexa executable: {e}")))?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

#[tool_router(router = router_admin, vis = "pub(crate)")]
impl IndexaMcp {
    /// Index statistics (entry + chunk counts).
    #[tool(
        description = "Return index statistics: total indexed entries and embedded chunks.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn get_stats(&self) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let entries = store.entry_count().map_err(mcp_err)?;
        let chunks = store.chunk_count().map_err(mcp_err)?;
        // Lead with the server version so an agent can tell when it's talking to a
        // stale binary (the v0.39 honesty fix: a 9-version-behind MCP served wrong
        // answers silently).
        let mut out = format!(
            "Indexa MCP v{}.  {entries} indexed entries, {chunks} chunks.",
            env!("CARGO_PKG_VERSION")
        );
        // Version skew: if the installed desktop app is newer than this binary, the
        // agent is talking to a stale MCP server (the trap where the app updated but
        // the CLI it spawns didn't). Fail-open — only the harmful "behind" case prints.
        if let Some(msg) = indexa_update::detect_skew(env!("CARGO_PKG_VERSION"))
            .advice(indexa_update::Surface::Mcp)
        {
            out.push_str(&format!("\n\u{26A0} {msg}"));
        }
        // Index freshness: warn when the newest indexed chunk is old, so the agent
        // knows the answers may predate recent code/file changes and can re-index.
        if let Ok(Some(ts)) = store.last_indexed_at() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(ts);
            let days = ((now - ts) / 86_400).max(0);
            if days >= 7 {
                out.push_str(&format!(
                    "\n\u{26A0} Index last updated {days} days ago — it may miss recent changes. \
                     Run `indexa index <root>` (or enable watch) to refresh before relying on answers."
                ));
            } else {
                out.push_str(&format!("\nIndex last updated {days}d ago."));
            }
        }
        // Measured token savings (approximate by definition — see store::usage);
        // best-effort, so a telemetry read failure can't fail the stats call. Only
        // meaningful when the index is fresh (see the freshness note above).
        if let Some(line) = store
            .usage_summary(indexa_core::store::USAGE_WEEK_SECS)
            .ok()
            .and_then(|u| u.savings_line())
        {
            out.push('\n');
            out.push_str(&line);
        }
        Ok(ok_text(out))
    }

    /// List the file formats Indexa can parse + their support level.
    #[tool(
        description = "List the file formats Indexa understands, each with its support level \
                       (full = text extracted, metadata = listing/EXIF only, stub = recognised \
                       but not extracted, textfallback = sniffed as text) and MIME type. Lets an \
                       agent check whether a file type will be indexed before adding it to scope.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn list_supported_formats(&self) -> Result<CallToolResult, ErrorData> {
        let formats = indexa_parsers::registry::Registry::new().supported_formats();
        let mut out = String::from("Indexa supported formats:\n");
        for f in &formats {
            let ext = if f.extension.starts_with('(') {
                f.extension.clone()
            } else {
                format!(".{}", f.extension)
            };
            out.push_str(&format!(
                "  {ext:<14} {:<12} {}\n",
                f.support_level,
                f.mime.as_deref().unwrap_or("—")
            ));
        }
        Ok(ok_text(out))
    }

    /// Report the effective Indexa configuration (models, retrieval, scan) — no secrets.
    #[tool(
        description = "Return Indexa's effective configuration: embedding + describer models, \
                       retrieval defaults (mode, top_k, agentic), chunking, scan ignore rules, \
                       and parser caps. API keys are NEVER included. Read-only — use it to \
                       understand how retrieval is tuned before asking or searching.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn query_config(&self) -> Result<CallToolResult, ErrorData> {
        let c = &self.config;
        // Deliberately excludes `api_keys` — secrets are never returned over a tool.
        let mode = format!("{:?}", c.retrieval.hybrid).to_lowercase();
        let out = format!(
            "Embedding: {} / {} (dim {})\n\
             Describer: {} / {} (file: {}, dir: {}; passes first/refresh: {}/{})\n\
             Retrieval: mode={mode}, top_k={}, rerank={}, agentic={} (max {} steps), \
             use_weights={}, context_budget={} bytes\n\
             Chunking:  {:?}, size {}, overlap {}\n\
             Scan:      respect_gitignore={}, auto_reindex={}, ignore=[{}]\n\
             Parsers:   max_file_mb={}, pdf_backend={}, image_caption={}, \
             audio_transcribe={}, video_caption={}",
            c.embedding.provider,
            c.embedding.model,
            c.embedding.dim,
            c.describer.provider,
            c.describer.model,
            c.describer.file_model,
            c.describer.dir_model,
            c.describer.passes_first,
            c.describer.passes_refresh,
            c.retrieval.top_k,
            c.retrieval.rerank,
            c.retrieval.agentic,
            c.retrieval.agentic_max_steps,
            c.retrieval.use_weights,
            c.retrieval.context_budget,
            c.chunking.strategy,
            c.chunking.size,
            c.chunking.overlap,
            c.scan.respect_gitignore,
            c.scan.auto_reindex,
            c.scan.ignore.join(", "),
            c.parsers.max_file_mb,
            c.parsers.pdf.backend,
            c.parsers.image.caption,
            c.parsers.audio.transcribe,
            c.parsers.video.caption,
        );
        Ok(ok_text(out))
    }

    /// Garbage-collect orphaned rows (chunks/summaries left behind after a root was removed).
    #[tool(
        description = "Garbage-collect orphaned index rows — chunks and summaries left behind after their files/roots were removed. Returns how many rows were pruned. Safe: only removes rows with no matching entry.",
        annotations(destructive_hint = true)
    )]
    pub(crate) async fn prune(&self) -> Result<CallToolResult, ErrorData> {
        let mut store = self.store()?;
        let counts = store.prune_orphans().map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Pruned {} orphaned chunk(s), {} stale queue row(s), {} summary row(s), {} \
             classification(s), and {} app detection(s).",
            counts.chunks,
            counts.queue,
            counts.summaries,
            counts.classifications,
            counts.directory_apps
        )))
    }

    /// Trigger a full scan → deep-index → summarize pipeline on a path.
    #[tool(
        description = "Start an `indexa index <path>` run: scan files, compute embeddings, \
                       and generate summaries. Runs as a background subprocess and returns \
                       when indexing is complete. Use before asking questions about new or \
                       changed files.",
        annotations(destructive_hint = true)
    )]
    pub(crate) async fn trigger_index(
        &self,
        params: Parameters<TriggerIndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = params.0.path;
        // Spawn `indexa index <path>` as a subprocess, using THIS server's own binary (not a
        // bare PATH lookup) so we re-invoke the exact `indexa` serving this MCP session. Both
        // processes open the same DB via WAL + 5s busy_timeout, which handles contention safely.
        let output = tokio::process::Command::new(indexa_exe()?)
            .args(["index", &path])
            .output()
            .await
            .map_err(|e| mcp_err(format!("failed to spawn `indexa index`: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            let summary = if stdout.trim().is_empty() {
                format!("indexa index {path} completed successfully.")
            } else {
                stdout.trim().to_owned()
            };
            Ok(ok_text(summary))
        } else {
            Err(mcp_err(format!(
                "indexa index {path} failed (exit {:?}):\n{}",
                output.status.code(),
                if stderr.trim().is_empty() {
                    &stdout
                } else {
                    &stderr
                }
                .trim()
            )))
        }
    }

    /// Persist a learned fact as a Markdown note in the Indexa data directory, attach it
    /// to an existing pack, and immediately index it — so it becomes searchable via
    /// `search`, `ask`, and `export_pack` right away.
    ///
    /// This is the **write-back** counterpart to retrieval: an AI caller that discovers
    /// something new (a bug root-cause, a design decision, a meeting outcome) can persist
    /// it here so the knowledge survives the session and enriches future context.
    ///
    /// The pack must already exist — create it first with `create_pack`.
    /// Notes are plain Markdown files; they inherit secret redaction on `export_pack`
    /// automatically (they live inside the pack). Re-submitting the same title + body is
    /// idempotent (same file is overwritten in place).
    #[tool(
        description = "Write a Markdown note to the Indexa data directory, attach it to an \
                       existing pack, and index it immediately so it is searchable. Use this \
                       to persist learned facts (design decisions, bug root-causes, meeting \
                       outcomes) that should survive the session. The pack must already exist \
                       — call `create_pack` first. Re-submitting the same title+body is \
                       idempotent. Notes are redacted on `export_pack` like any other file.",
        annotations(destructive_hint = true)
    )]
    pub(crate) async fn add_note(
        &self,
        params: Parameters<AddNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let AddNoteParams { pack, title, body } = params.0;

        // Derive data_dir from db_path (db_path = <data_dir>/index.db).
        let data_dir = self
            .db_path
            .parent()
            .ok_or_else(|| mcp_err("db_path has no parent directory"))?;

        // Verify the pack exists before writing anything.
        let mut store = Store::open(&self.db_path).map_err(mcp_err)?;
        let pack_rec = store.pack_by_name(&pack).map_err(mcp_err)?.ok_or_else(|| {
            mcp_err(format!(
                "no pack named \"{pack}\" — create it first with `create_pack`"
            ))
        })?;

        // Write the note file (idempotent: same title+body → same filename).
        let note_path = indexa_core::notes::write_note_file(data_dir, &pack, &title, &body)
            .map_err(|e| mcp_err(format!("writing note: {e}")))?;

        let note_path_str = note_path.to_string_lossy().into_owned();

        // Register the note in the pack so `search_pack` / `export_pack` find it.
        store
            .add_pack_paths(&pack_rec.id, std::slice::from_ref(&note_path_str))
            .map_err(mcp_err)?;

        // Index immediately: scan + deep-embed + summarize the notes directory so the
        // note is searchable right away. Best-effort — a failure here still means the
        // note is written and pack-registered; the caller can trigger re-indexing later.
        let notes_dir = data_dir.join("notes");
        let notes_dir_str = notes_dir.to_string_lossy().into_owned();
        let index_result = tokio::process::Command::new(indexa_exe()?)
            .args(["index", &notes_dir_str])
            .output()
            .await;

        let index_note = match index_result {
            Ok(out) if out.status.success() => String::new(),
            Ok(out) => {
                let msg = String::from_utf8_lossy(&out.stderr);
                format!(
                    "\n⚠ Indexing note failed (exit {:?}): {msg}",
                    out.status.code()
                )
            }
            Err(e) => format!("\n⚠ Could not spawn `indexa index`: {e}"),
        };

        Ok(ok_text(format!(
            "Note \"{title}\" added to pack \"{pack}\" and indexed.\nFile: {note_path_str}{index_note}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::indexa_exe;

    /// `trigger_index` / `add_note` must spawn the server's OWN binary, not a bare `"indexa"`
    /// PATH lookup. Assert the resolver returns a concrete, absolute, existing path.
    #[test]
    fn indexa_exe_resolves_to_the_running_binary_not_a_bare_name() {
        let exe = indexa_exe().expect("current_exe is resolvable under test");
        assert!(
            exe.is_absolute(),
            "must be an absolute path (a real executable), not a bare `indexa` PATH lookup: {exe:?}"
        );
        assert!(
            exe.exists(),
            "resolved executable should exist on disk: {exe:?}"
        );
        assert_ne!(
            exe.as_os_str(),
            "indexa",
            "must not regress to the bare command name"
        );
    }
}
