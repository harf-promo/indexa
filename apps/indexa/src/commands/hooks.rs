//! Claude Code hook installer + hook-script backend (3.1) — the distribution play for
//! getting Indexa's context to Claude Code passively (session start, tool use) instead of
//! only on an agent's explicit tool calls.
//!
//! Two pieces:
//! - `indexa install-hooks` generates the hook config block and PRINTS it by default;
//!   `--write` merges it into the current project's `.claude/settings.json` (a `.bak` of the
//!   original is kept). Never touches the user's global `~/.claude/settings.json`.
//! - `indexa mcp-hook <session-start|pre-tool-use>` is the hook script itself, invoked by
//!   Claude Code. It reads the event JSON on stdin (best-effort), prints the
//!   `hookSpecificOutput` JSON Claude Code expects, and ALWAYS succeeds: a context-injection
//!   hook must never block or fail the caller's turn (fail-open throughout — see
//!   https://code.claude.com/docs/en/hooks.md for the wire format this implements).

use anyhow::{bail, Context, Result};
use indexa_core::config::HybridMode;
use indexa_core::store::Store;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

use super::helpers::index_db_path;

/// Which hook event `indexa mcp-hook` is being invoked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookEvent {
    SessionStart,
    PreToolUse,
}

impl HookEvent {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "session-start" => Ok(Self::SessionStart),
            "pre-tool-use" => Ok(Self::PreToolUse),
            other => bail!("unknown hook event '{other}' — expected session-start or pre-tool-use"),
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::PreToolUse => "PreToolUse",
        }
    }
}

/// `indexa install-hooks` — print (default) or write (`--write`) the Claude Code hook config
/// block for the current project. `with_pretooluse` additionally includes a `PreToolUse`
/// hook on `Grep` that surfaces related indexed context for the search pattern.
pub(crate) async fn cmd_install_hooks(write: bool, with_pretooluse: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve indexa's own executable path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let exe = exe.to_string_lossy().into_owned();

    let mut additions: Vec<(&'static str, Value)> = vec![(
        "SessionStart",
        json!({
            "hooks": [
                { "type": "command", "command": format!("{exe} mcp-hook session-start"), "timeout": 10 }
            ]
        }),
    )];
    if with_pretooluse {
        additions.push((
            "PreToolUse",
            json!({
                "matcher": "Grep",
                "hooks": [
                    { "type": "command", "command": format!("{exe} mcp-hook pre-tool-use"), "timeout": 10 }
                ]
            }),
        ));
    }

    let path = Path::new(".claude").join("settings.json");
    install_hooks_json(&path, &additions, write)
}

/// Merge `additions` (event name -> hook-group object) into `root.hooks.<event>`, appending
/// each new group to that event's array rather than replacing it — a user's own unrelated
/// hooks for the same event (or a different matcher) are preserved. Idempotent: a group with
/// the same `command` already present is skipped, so re-running never duplicates entries.
fn merge_hooks(mut root: Value, additions: &[(&'static str, Value)]) -> Result<Value> {
    let Value::Object(map) = &mut root else {
        bail!("config root is not a JSON object — refusing to overwrite it");
    };
    let hooks_entry = map.entry("hooks".to_owned()).or_insert_with(|| json!({}));
    let Value::Object(hooks_map) = hooks_entry else {
        bail!("'hooks' is not a JSON object — refusing to overwrite it");
    };
    for (event, group) in additions {
        let arr_entry = hooks_map
            .entry((*event).to_owned())
            .or_insert_with(|| json!([]));
        let Value::Array(arr) = arr_entry else {
            bail!("'hooks.{event}' is not a JSON array — refusing to overwrite it");
        };
        let new_cmd = group["hooks"][0]["command"].as_str().unwrap_or_default();
        let already_present = arr.iter().any(|existing| {
            existing["hooks"]
                .as_array()
                .map(|hs| hs.iter().any(|h| h["command"] == new_cmd))
                .unwrap_or(false)
        });
        if !already_present {
            arr.push(group.clone());
        }
    }
    Ok(root)
}

/// Merge `additions` into the hook config at `path` — print-only unless `write` is set,
/// mirroring `mcp_install.rs`'s `install_json` (backup-then-write, refuse non-JSON/non-object
/// shapes) but inverted default: hooks affect what gets injected into every session, so the
/// safer default is showing what WOULD change rather than applying it.
fn install_hooks_json(path: &Path, additions: &[(&'static str, Value)], write: bool) -> Result<()> {
    let existed = path.exists();
    let original = if existed {
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?
    } else {
        String::new()
    };
    let root: Value = if original.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&original).with_context(|| {
            format!(
                "{} is not valid JSON — fix or remove it, then re-run",
                path.display()
            )
        })?
    };

    let merged = merge_hooks(root.clone(), additions)?;

    if !write {
        println!("[preview] would write {}:", path.display());
        println!("{}", serde_json::to_string_pretty(&merged)?);
        println!();
        println!(
            "Re-run with --write to apply (a .bak of the original is kept if the file exists)."
        );
        return Ok(());
    }

    if existed && merged == root {
        println!("hooks already installed in {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
    }
    if existed {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, &bak).with_context(|| format!("cannot back up {}", path.display()))?;
        println!("backed up original to {}.bak", path.display());
    }
    let pretty = format!("{}\n", serde_json::to_string_pretty(&merged)?);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, pretty).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    println!("hooks installed in {}", path.display());
    println!("Restart Claude Code (or start a new session) for SessionStart to take effect.");
    Ok(())
}

/// `indexa mcp-hook <event>` — the hook script Claude Code invokes. ALWAYS returns `Ok(())`:
/// a context-injection hook must never block or fail the caller's turn, so every internal
/// step degrades to "no additional context" rather than propagating an error.
pub(crate) async fn cmd_mcp_hook(event: HookEvent) -> Result<()> {
    // Resolved once here (not inside the *_for helpers) so those stay unit-testable against
    // an arbitrary path without ever touching the real user data directory.
    let db_path = index_db_path().ok();
    let context = match event {
        HookEvent::SessionStart => db_path.as_deref().and_then(session_start_context_for),
        HookEvent::PreToolUse => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).ok();
            let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
            db_path
                .as_deref()
                .and_then(|p| pre_tool_use_context_for(&payload, p))
        }
    };
    let out = match context.filter(|c| !c.is_empty()) {
        Some(ctx) => json!({
            "hookSpecificOutput": {
                "hookEventName": event.wire_name(),
                "additionalContext": ctx,
            }
        }),
        None => json!({}),
    };
    println!("{out}");
    Ok(())
}

/// A short freshness blurb for `SessionStart`: entry/chunk counts, indexed roots, and how
/// long ago the index was last touched. `None` when there's no index at `db_path` (nothing
/// useful to say) or any step fails (fail-open). Takes `db_path` explicitly (rather than
/// resolving it internally) so this is testable against a tempdir path — never the real
/// user data directory.
fn session_start_context_for(db_path: &Path) -> Option<String> {
    if !db_path.exists() {
        return None;
    }
    let store = Store::open(db_path).ok()?;
    let entries = store.entry_count().ok()?;
    let chunks = store.chunk_count().ok()?;
    let roots = store.root_paths().unwrap_or_default();
    let freshness = store
        .last_indexed_at()
        .ok()
        .flatten()
        .map(age_blurb)
        .unwrap_or_else(|| "never deep-indexed".to_owned());
    let roots_blurb = if roots.is_empty() {
        "(none yet)".to_owned()
    } else {
        roots.join(", ")
    };
    Some(format!(
        "Indexa has a local index for this machine: {entries} entries, {chunks} chunks. \
         Indexed roots: {roots_blurb}. Last deep-indexed: {freshness}. Prefer the `indexa` \
         MCP tools (search/ask/dependencies/blast_radius/...) over re-reading files from \
         scratch when the question is about this codebase."
    ))
}

/// Related indexed context for `PreToolUse` on a `Grep` call: sparse-only search (no
/// embedder/Ollama dependency, so this stays fast and works even when local models aren't
/// running) against the grep pattern, top 5 hits. `None` when there's nothing to add or any
/// step fails (fail-open). Takes `db_path` explicitly for the same reason as
/// [`session_start_context_for`] — testable against a tempdir path, never the real user
/// data directory.
fn pre_tool_use_context_for(payload: &Value, db_path: &Path) -> Option<String> {
    let pattern = payload.get("tool_input")?.get("pattern")?.as_str()?;
    if pattern.trim().is_empty() {
        return None;
    }
    if !db_path.exists() {
        return None;
    }
    let store = Store::open(db_path).ok()?;
    let hits = store
        .hybrid_search(pattern, None, &HybridMode::Sparse, None, 5, 60.0)
        .ok()?;
    if hits.is_empty() {
        return None;
    }
    let body = hits
        .iter()
        .map(|h| format!("- {} ({})", h.entry_path, h.heading))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Indexa already has indexed context related to this search:\n{body}\n\
         (consider `search`/`ask` via the indexa MCP tools instead of grepping further)"
    ))
}

/// `"Xm ago"` / `"Xh ago"` / `"Xd ago"` for a Unix-epoch-seconds timestamp.
fn age_blurb(indexed_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(indexed_at);
    let secs = (now - indexed_at).max(0);
    if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "indexa-hooks-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn session_start_group(cmd: &str) -> (&'static str, Value) {
        (
            "SessionStart",
            json!({ "hooks": [ { "type": "command", "command": cmd, "timeout": 10 } ] }),
        )
    }

    #[test]
    fn hook_event_parse_accepts_known_and_rejects_unknown() {
        assert_eq!(
            HookEvent::parse("session-start").unwrap(),
            HookEvent::SessionStart
        );
        assert_eq!(
            HookEvent::parse("pre-tool-use").unwrap(),
            HookEvent::PreToolUse
        );
        assert!(HookEvent::parse("bogus").is_err());
    }

    #[test]
    fn preview_mode_writes_nothing() {
        let dir = TempDir::new();
        let path = dir.file("settings.json");
        install_hooks_json(
            &path,
            &[session_start_group("indexa mcp-hook session-start")],
            false,
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn write_mode_creates_the_hooks_block() {
        let dir = TempDir::new();
        let path = dir.file("settings.json");
        install_hooks_json(
            &path,
            &[session_start_group("indexa mcp-hook session-start")],
            true,
        )
        .unwrap();
        let v = read_json(&path);
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "indexa mcp-hook session-start"
        );
        assert!(!dir.file("settings.json.bak").exists());
    }

    #[test]
    fn write_mode_preserves_unrelated_existing_hooks_and_keys() {
        let dir = TempDir::new();
        let path = dir.file("settings.json");
        std::fs::write(
            &path,
            r#"{
                "hooks": {
                    "SessionStart": [
                        { "hooks": [ { "type": "command", "command": "my-other-hook.sh" } ] }
                    ]
                },
                "someOtherKey": "value"
            }"#,
        )
        .unwrap();

        install_hooks_json(
            &path,
            &[session_start_group("indexa mcp-hook session-start")],
            true,
        )
        .unwrap();

        let v = read_json(&path);
        assert_eq!(v["someOtherKey"], "value");
        let arr = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "the user's existing hook must survive");
        assert!(arr
            .iter()
            .any(|g| g["hooks"][0]["command"] == "my-other-hook.sh"));
        assert!(arr
            .iter()
            .any(|g| g["hooks"][0]["command"] == "indexa mcp-hook session-start"));

        let bak = read_json(&dir.file("settings.json.bak"));
        assert_eq!(bak["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn rerun_is_idempotent_and_does_not_duplicate() {
        let dir = TempDir::new();
        let path = dir.file("settings.json");
        let additions = [session_start_group("indexa mcp-hook session-start")];
        install_hooks_json(&path, &additions, true).unwrap();
        install_hooks_json(&path, &additions, true).unwrap();
        let v = read_json(&path);
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn invalid_json_is_an_error_not_a_clobber() {
        let dir = TempDir::new();
        let path = dir.file("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let additions = [session_start_group("indexa mcp-hook session-start")];
        assert!(install_hooks_json(&path, &additions, true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn pre_tool_use_context_for_returns_none_without_a_pattern() {
        let dir = TempDir::new();
        let db_path = dir.file("index.db");
        assert!(pre_tool_use_context_for(&json!({}), &db_path).is_none());
        assert!(pre_tool_use_context_for(&json!({ "tool_input": {} }), &db_path).is_none());
        assert!(
            pre_tool_use_context_for(&json!({ "tool_input": { "pattern": "" } }), &db_path)
                .is_none()
        );
    }

    #[test]
    fn pre_tool_use_context_for_returns_none_when_no_index_exists_at_the_given_path() {
        let dir = TempDir::new();
        let db_path = dir.file("index.db"); // never created
        let payload = json!({ "tool_input": { "pattern": "some search term" } });
        assert!(pre_tool_use_context_for(&payload, &db_path).is_none());
    }

    #[test]
    fn pre_tool_use_context_for_finds_a_real_sparse_hit_in_a_scratch_index() {
        let dir = TempDir::new();
        let db_path = dir.file("index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .upsert_entries(&[indexa_core::walker::Entry {
                    path: std::path::PathBuf::from("/proj/auth.rs"),
                    kind: indexa_core::walker::EntryKind::File,
                    size: 10,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
            store
                .upsert_chunks(&[indexa_core::store::ChunkRecord {
                    entry_path: "/proj/auth.rs".into(),
                    seq: 0,
                    heading: "validate_token".into(),
                    text: "fn validate_token(t: &str) -> bool { check_signature(t) }".into(),
                    language: None,
                    embedding: None,
                    embed_model: None,
                    content_hash: None,
                }])
                .unwrap();
        }
        let payload = json!({ "tool_input": { "pattern": "validate_token" } });
        let ctx = pre_tool_use_context_for(&payload, &db_path)
            .expect("a real chunk matching the pattern should produce context");
        assert!(ctx.contains("/proj/auth.rs"), "got: {ctx}");
    }

    #[test]
    fn session_start_context_for_returns_none_when_no_index_exists_at_the_given_path() {
        let dir = TempDir::new();
        let db_path = dir.file("index.db");
        assert!(session_start_context_for(&db_path).is_none());
    }

    #[test]
    fn session_start_context_for_reports_real_counts_and_roots() {
        let dir = TempDir::new();
        let db_path = dir.file("index.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .upsert_entries(&[indexa_core::walker::Entry {
                    path: std::path::PathBuf::from("/proj"),
                    kind: indexa_core::walker::EntryKind::Dir,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
        }
        let ctx = session_start_context_for(&db_path).expect("an index exists");
        assert!(ctx.contains("/proj"), "got: {ctx}");
        assert!(ctx.contains("never deep-indexed"), "got: {ctx}");
    }

    #[test]
    fn age_blurb_formats_minutes_hours_days() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(age_blurb(now - 120), "2m ago");
        assert_eq!(age_blurb(now - 7200), "2h ago");
        assert_eq!(age_blurb(now - 2 * 86_400), "2d ago");
    }
}
