use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Clients `indexa mcp install` knows how to configure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpClient {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    VsCode,
}

impl McpClient {
    fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" => Ok(Self::ClaudeCode),
            "claude-desktop" => Ok(Self::ClaudeDesktop),
            "cursor" => Ok(Self::Cursor),
            "vscode" => Ok(Self::VsCode),
            other => bail!(
                "unknown client '{other}' — expected one of: \
                 claude-code, claude-desktop, cursor, vscode"
            ),
        }
    }

    /// Canonical name, for echoing what was auto-detected.
    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::ClaudeDesktop => "claude-desktop",
            Self::Cursor => "cursor",
            Self::VsCode => "vscode",
        }
    }
}

/// Best-effort detection of which supported clients are installed, used when
/// `indexa mcp install` runs with no `--client`. Each probe is cheap and
/// side-effect-free: `claude` on PATH, a present Claude Desktop / Cursor config
/// directory, or a `.vscode` workspace in the current directory. A false
/// negative just means the user names the client explicitly — never destructive.
fn detect_installed_clients() -> Vec<McpClient> {
    let mut found = Vec::new();
    if find_on_path("claude").is_some() {
        found.push(McpClient::ClaudeCode);
    }
    let dir_exists = |p: Result<PathBuf>| {
        p.ok()
            .and_then(|p| p.parent().map(|d| d.is_dir()))
            .unwrap_or(false)
    };
    if dir_exists(claude_desktop_config_path()) {
        found.push(McpClient::ClaudeDesktop);
    }
    if dir_exists(cursor_config_path()) {
        found.push(McpClient::Cursor);
    }
    if Path::new(".vscode").is_dir() {
        found.push(McpClient::VsCode);
    }
    found
}

/// One-shot MCP registration: point each requested client at this binary.
/// `clients` arrives pre-split by clap (repeatable flag or comma list).
pub(crate) async fn cmd_mcp_install(clients: Vec<String>, dry_run: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve indexa's own executable path")?;
    // Canonicalize so the config survives PATH changes and symlinked installs;
    // fall back to the raw path if the filesystem refuses (e.g. odd mounts).
    let exe = exe.canonicalize().unwrap_or(exe);
    let exe = exe.to_string_lossy().into_owned();

    let mut parsed: Vec<McpClient> = Vec::new();
    if clients.is_empty() {
        // No --client: auto-detect installed clients and configure each one found.
        parsed = detect_installed_clients();
        if parsed.is_empty() {
            println!(
                "No supported MCP clients detected (looked for: `claude` on PATH, a Claude \
                 Desktop or Cursor config directory, and a ./.vscode workspace)."
            );
            println!("Name one explicitly, e.g.: indexa mcp install --client claude-code");
            return Ok(());
        }
        let names: Vec<&str> = parsed.iter().map(|c| c.label()).collect();
        println!("Auto-detected installed client(s): {}", names.join(", "));
    } else {
        for c in &clients {
            let client = McpClient::parse(c)?;
            if !parsed.contains(&client) {
                parsed.push(client);
            }
        }
    }

    let mut configured_any = false;
    for client in parsed {
        let done = match client {
            McpClient::ClaudeCode => install_claude_code(&exe, dry_run)?,
            McpClient::ClaudeDesktop => install_json(
                &claude_desktop_config_path()?,
                "mcpServers",
                &exe,
                dry_run,
                "claude-desktop",
            )?,
            McpClient::Cursor => install_json(
                &cursor_config_path()?,
                "mcpServers",
                &exe,
                dry_run,
                "cursor",
            )?,
            McpClient::VsCode => {
                // VS Code's MCP config is per-workspace and uses "servers", not "mcpServers".
                let path = Path::new(".vscode").join("mcp.json");
                println!("vscode: configuring the current workspace (./.vscode/mcp.json)");
                install_json(&path, "servers", &exe, dry_run, "vscode")?
            }
        };
        configured_any |= done;
    }

    if configured_any {
        println!();
        println!(
            "Verify: restart the client, then ask your agent: \
             \"using indexa, what's in <folder>?\""
        );
    }
    Ok(())
}

/// Set `<key>.indexa = {command, args: ["mcp"]}` in the JSON at `root`,
/// touching nothing else. Refuses (rather than clobbers) non-object shapes.
fn merge_server_entry(mut root: Value, key: &str, exe: &str) -> Result<Value> {
    let Value::Object(map) = &mut root else {
        bail!("config root is not a JSON object — refusing to overwrite it");
    };
    let servers = map.entry(key.to_owned()).or_insert_with(|| json!({}));
    let Value::Object(servers) = servers else {
        bail!("'{key}' is not a JSON object — refusing to overwrite it");
    };
    servers.insert(
        "indexa".to_owned(),
        json!({ "command": exe, "args": ["mcp"] }),
    );
    Ok(root)
}

/// Merge the indexa entry into the config file at `path`.
/// Missing file → start from {}. A `.bak` of the original is written only
/// when the file existed and is actually being changed.
/// Returns true when the entry is in place (false on dry-run).
fn install_json(path: &Path, key: &str, exe: &str, dry_run: bool, label: &str) -> Result<bool> {
    let existed = path.exists();
    let original = if existed {
        std::fs::read_to_string(path)
            .with_context(|| format!("{label}: cannot read {}", path.display()))?
    } else {
        String::new()
    };
    // Treat an empty/whitespace file like a missing one; broken JSON is an
    // error — silently replacing a user's hand-edited config would lose data.
    let root: Value = if original.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&original).with_context(|| {
            format!(
                "{label}: {} is not valid JSON — fix or remove it, then re-run",
                path.display()
            )
        })?
    };

    let merged = merge_server_entry(root.clone(), key, exe)?;

    if dry_run {
        println!("[dry-run] {label}: would write {}", path.display());
        println!("{}", serde_json::to_string_pretty(&merged)?);
        return Ok(false);
    }

    if existed && merged == root {
        println!("{label}: already configured ({})", path.display());
        return Ok(true);
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("{label}: cannot create {}", parent.display()))?;
        }
    }

    if existed {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, &bak)
            .with_context(|| format!("{label}: cannot back up {}", path.display()))?;
        println!("{label}: backed up original to {}.bak", path.display());
    }

    let pretty = format!("{}\n", serde_json::to_string_pretty(&merged)?);
    // Write-temp-then-rename: a crash or ENOSPC mid-write must never leave the
    // client's live config truncated (fs::write truncates before writing).
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, pretty)
        .with_context(|| format!("{label}: cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("{label}: cannot replace {}", path.display()))?;
    println!("{label}: registered indexa in {}", path.display());
    Ok(true)
}

/// Claude Code has its own registry command; prefer it so scope/validation
/// stay Claude Code's problem. If the binary is missing or the command fails,
/// print the exact command for the user instead of guessing at config files.
fn install_claude_code(exe: &str, dry_run: bool) -> Result<bool> {
    let manual = format!("claude mcp add --scope user indexa -- {exe} mcp");

    if dry_run {
        println!("[dry-run] claude-code: would run `{manual}`");
        return Ok(false);
    }

    let Some(claude) = find_on_path("claude") else {
        println!("claude-code: `claude` not found on PATH — run this yourself:");
        println!("  {manual}");
        return Ok(false);
    };

    let result = std::process::Command::new(&claude)
        .args(["mcp", "add", "--scope", "user", "indexa", "--", exe, "mcp"])
        .output();
    match result {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !stdout.trim().is_empty() {
                println!("{}", stdout.trim());
            }
            println!(
                "claude-code: registered indexa (scope: user — config managed by the \
                 `claude` CLI; undo with `claude mcp remove indexa`)"
            );
            Ok(true)
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                eprintln!("{}", stderr.trim());
            }
            println!("claude-code: `claude mcp add` failed — run this yourself:");
            println!("  {manual}");
            Ok(false)
        }
        Err(e) => {
            println!("claude-code: could not run `claude` ({e}) — run this yourself:");
            println!("  {manual}");
            Ok(false)
        }
    }
}

/// macOS: ~/Library/Application Support/Claude/claude_desktop_config.json
/// Windows: %APPDATA%\Claude\claude_desktop_config.json
/// Linux: ~/.config/Claude/claude_desktop_config.json
/// `BaseDirs::config_dir()` resolves to exactly those roots per platform.
fn claude_desktop_config_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("cannot resolve the user config directory")?;
    Ok(base
        .config_dir()
        .join("Claude")
        .join("claude_desktop_config.json"))
}

fn cursor_config_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("cannot resolve the home directory")?;
    Ok(base.home_dir().join(".cursor").join("mcp.json"))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let exact = dir.join(name);
        if exact.is_file() {
            return Some(exact);
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Minimal tempdir (no tempfile dep): unique per process+counter,
    /// removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "indexa-mcp-install-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self, name: &str) -> PathBuf {
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

    #[test]
    fn merge_preserves_other_servers_and_keys() {
        let dir = TempDir::new();
        let path = dir.file("claude_desktop_config.json");
        std::fs::write(
            &path,
            r#"{
                "mcpServers": {
                    "other": { "command": "other-tool", "args": ["--serve"] }
                },
                "globalShortcut": "Cmd+K"
            }"#,
        )
        .unwrap();

        let wrote =
            install_json(&path, "mcpServers", "/usr/local/bin/indexa", false, "test").unwrap();
        assert!(wrote);

        let v = read_json(&path);
        assert_eq!(v["globalShortcut"], "Cmd+K");
        assert_eq!(v["mcpServers"]["other"]["command"], "other-tool");
        assert_eq!(
            v["mcpServers"]["indexa"]["command"],
            "/usr/local/bin/indexa"
        );
        assert_eq!(v["mcpServers"]["indexa"]["args"], json!(["mcp"]));

        // .bak holds the pre-merge original.
        let bak = read_json(&dir.file("claude_desktop_config.json.bak"));
        assert!(bak["mcpServers"].get("indexa").is_none());
        assert_eq!(bak["globalShortcut"], "Cmd+K");
    }

    #[test]
    fn missing_file_starts_from_empty_object_without_bak() {
        let dir = TempDir::new();
        let path = dir.file("nested").join("mcp.json");

        let wrote = install_json(&path, "mcpServers", "/bin/indexa", false, "test").unwrap();
        assert!(wrote);

        let v = read_json(&path);
        assert_eq!(v["mcpServers"]["indexa"]["command"], "/bin/indexa");
        assert!(!dir.file("nested").join("mcp.json.bak").exists());
    }

    #[test]
    fn rerun_is_idempotent_and_skips_rewrite() {
        let dir = TempDir::new();
        let path = dir.file("mcp.json");

        install_json(&path, "servers", "/bin/indexa", false, "test").unwrap();
        let first = std::fs::read_to_string(&path).unwrap();

        let wrote = install_json(&path, "servers", "/bin/indexa", false, "test").unwrap();
        assert!(wrote); // entry is in place
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
        // No-change re-run must not create a backup of an already-correct file.
        assert!(!dir.file("mcp.json.bak").exists());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = TempDir::new();
        let path = dir.file("mcp.json");

        let wrote = install_json(&path, "mcpServers", "/bin/indexa", true, "test").unwrap();
        assert!(!wrote);
        assert!(!path.exists());
    }

    #[test]
    fn invalid_json_is_an_error_not_a_clobber() {
        let dir = TempDir::new();
        let path = dir.file("mcp.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert!(install_json(&path, "mcpServers", "/bin/indexa", false, "test").is_err());
        // Original is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn non_object_servers_key_is_refused() {
        let root: Value = json!({ "mcpServers": ["not", "a", "map"] });
        assert!(merge_server_entry(root, "mcpServers", "/bin/indexa").is_err());
    }
}
