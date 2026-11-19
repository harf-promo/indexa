//! Admin tools: `get_stats`, `prune`, `trigger_index`, and `add_note`.

use indexa_core::store::Store;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;
use std::process::Stdio;

use crate::{mcp_err, mcp_invalid, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TriggerIndexParams {
    /// Absolute path inside an already-indexed root to scan, deep-index, and summarize.
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
    /// Optional (2.6): anchor this note to a code path or bare symbol name, so
    /// `dependencies`/`blast_radius`/`symbol_context` surface it inline when that
    /// path/symbol comes up later. A value starting with `/` is treated as a path;
    /// anything else as a symbol name.
    #[serde(default)]
    pub anchor: Option<String>,
}

const DEFAULT_TRIGGER_INDEX_TIMEOUT_SECS: u64 = 1800;

fn trigger_index_timeout() -> std::time::Duration {
    std::env::var("INDEXA_TRIGGER_INDEX_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(
            DEFAULT_TRIGGER_INDEX_TIMEOUT_SECS,
        ))
}

async fn run_trigger_child(
    child: &mut tokio::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, ErrorData> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.as_std_mut().process_group(0);
    }
    child.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    let job = ChildJob::new().map_err(|e| mcp_err(format!("failed to create child job: {e}")))?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        child
            .as_std_mut()
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }
    #[allow(unused_mut)]
    let mut process = child
        .spawn()
        .map_err(|e| mcp_err(format!("failed to spawn `indexa index`: {e}")))?;
    #[cfg(windows)]
    {
        if let Err(e) = assign_or_terminate_child(&mut process, |process| job.assign(process)) {
            return Err(mcp_err(format!(
                "failed to assign `indexa index` to child job: {e}"
            )));
        }
        if let Err(e) = ChildJob::resume_child(&process) {
            let e = terminate_unassigned_child(&mut process, e);
            return Err(mcp_err(format!(
                "failed to resume `indexa index` child: {e}"
            )));
        }
    }
    #[cfg(unix)]
    let mut group = process.id().map(|pid| ProcessGroupGuard {
        pid: pid as i32,
        armed: true,
    });
    let result = tokio::time::timeout(timeout, process.wait_with_output()).await;
    #[cfg(unix)]
    if result.is_err() {
        drop(group); // Drop kills the whole process group, including grandchildren.
    } else if let Some(group) = &mut group {
        group.armed = false;
    }
    result
        .map_err(|_| {
            mcp_err(format!(
                "trigger_index child timed out after {}s",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| mcp_err(format!("failed to spawn `indexa index`: {e}")))
}

/// Unix timeout/cancellation cleanup for the entire spawned process group.
#[cfg(unix)]
struct ProcessGroupGuard {
    pid: i32,
    armed: bool,
}

#[cfg(windows)]
struct ChildJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
fn terminate_unassigned_child(
    child: &mut tokio::process::Child,
    error: std::io::Error,
) -> std::io::Error {
    let _ = child.start_kill();
    error
}

#[cfg(windows)]
fn assign_or_terminate_child(
    child: &mut tokio::process::Child,
    assign: impl FnOnce(&tokio::process::Child) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match assign(child) {
        Ok(()) => Ok(()),
        Err(error) => Err(terminate_unassigned_child(child, error)),
    }
}

#[cfg(windows)]
impl ChildJob {
    fn new() -> std::io::Result<Self> {
        use std::{mem::size_of, os::windows::io::FromRawHandle};
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::JobObjects::{
                CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
        };
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let owned = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.cast()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(owned))
    }

    fn assign(&self, child: &tokio::process::Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        let process_handle = child.raw_handle().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "child process already exited")
        })?;
        let ok = unsafe {
            AssignProcessToJobObject(self.0.as_raw_handle().cast(), process_handle.cast())
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn resume_child(child: &tokio::process::Child) -> std::io::Result<()> {
        use std::{
            mem::size_of,
            os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        };
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                    THREADENTRY32,
                },
                Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
            },
        };
        let pid = child.id().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "child process already exited")
        })?;
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let _snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.cast()) };
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut found = false;
        let mut last_error = None;
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    let thread = unsafe { OwnedHandle::from_raw_handle(thread.cast()) };
                    if unsafe { ResumeThread(thread.as_raw_handle().cast()) } != u32::MAX {
                        found = true;
                        break;
                    }
                    last_error = Some(std::io::Error::last_os_error());
                }
            }
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if found {
            Ok(())
        } else {
            Err(last_error.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "child primary thread not found",
                )
            }))
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
    }
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

/// Reject a `trigger_index` path that is not a plain absolute path to something that exists.
/// `indexa index` takes global flags (`--config=<file>`, which can register a parser
/// preprocessor command to run) anywhere on its command line, so a caller-supplied path that
/// starts with `-` would be parsed as a flag instead of a path.
fn validate_index_path(
    path: &str,
    indexed_roots: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, String> {
    if path.is_empty() || path.starts_with('-') {
        return Err(format!(
            "invalid path {path:?}: must be a non-empty absolute path and must not start with '-'"
        ));
    }
    if path.contains('\0') {
        return Err("invalid path: contains a NUL byte".to_owned());
    }
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err(format!("path must be absolute: {path}"));
    }
    if !p.exists() {
        return Err(format!("path does not exist: {path}"));
    }
    let canonical = p
        .canonicalize()
        .map_err(|e| format!("cannot resolve path {path}: {e}"))?;
    let inside = indexed_roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| canonical.starts_with(root));
    if !inside {
        return Err(format!(
            "path is outside every indexed root: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn open_validated_index_path(
    canonical_path: &std::path::Path,
    indexed_roots: &[std::path::PathBuf],
) -> Result<std::fs::File, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let before = canonical_path
        .metadata()
        .map_err(|e| format!("cannot inspect path {}: {e}", canonical_path.display()))?;
    if !before.is_dir() && !before.is_file() {
        return Err("trigger_index requires a regular file or directory".into());
    }
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if before.is_dir() {
            libc::O_DIRECTORY
        } else {
            0
        };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(canonical_path)
        .map_err(|e| format!("cannot safely open path {}: {e}", canonical_path.display()))?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    if (opened.is_dir() != before.is_dir())
        || (!opened.is_dir() && !opened.is_file())
        || (opened.dev(), opened.ino()) != (before.dev(), before.ino())
    {
        return Err("path changed while it was being validated".into());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let fd_path = std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let opened_path = fd_path
            .canonicalize()
            .map_err(|e| format!("cannot resolve opened directory: {e}"))?;
        if !indexed_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| opened_path.starts_with(root))
        {
            return Err("opened path is outside every indexed root".into());
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let fd_path = std::path::PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()));
        let opened_path = fd_path
            .canonicalize()
            .map_err(|e| format!("cannot resolve opened directory: {e}"))?;
        if !indexed_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| opened_path.starts_with(root))
        {
            return Err("opened path is outside every indexed root".into());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = indexed_roots;
    Ok(file)
}

#[cfg(windows)]
fn open_validated_index_path(
    canonical_path: &std::path::Path,
    indexed_roots: &[std::path::PathBuf],
) -> Result<std::fs::File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_SHARE_READ, FILE_SHARE_WRITE,
        VOLUME_NAME_DOS,
    };

    fn normalized(path: &str) -> String {
        let path = path.strip_prefix(r"\\?\UNC\").map_or_else(
            || path.strip_prefix(r"\\?\").unwrap_or(path).to_owned(),
            |unc| format!(r"\\{unc}"),
        );
        path.trim_end_matches(['\\', '/']).to_lowercase()
    }

    fn final_path(file: &std::fs::File) -> Result<std::path::PathBuf, String> {
        let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut buffer = vec![0u16; 512];
        loop {
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    handle,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            } as usize;
            if len == 0 {
                return Err(format!(
                    "cannot resolve opened path: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if len < buffer.len() {
                buffer.truncate(len);
                let path = String::from_utf16(&buffer)
                    .map_err(|e| format!("opened path is not valid UTF-16: {e}"))?;
                return Ok(std::path::PathBuf::from(path));
            }
            buffer.resize(len + 1, 0);
        }
    }

    let before = canonical_path
        .metadata()
        .map_err(|e| format!("cannot inspect path {}: {e}", canonical_path.display()))?;
    if !before.is_dir() && !before.is_file() {
        return Err("trigger_index requires a regular file or directory".into());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(canonical_path)
        .map_err(|e| format!("cannot safely open path {}: {e}", canonical_path.display()))?;

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe {
        GetFileInformationByHandle(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            &mut info,
        )
    };
    if ok == 0 {
        return Err(format!(
            "cannot inspect opened path: {}",
            std::io::Error::last_os_error()
        ));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("trigger_index refuses symlinks and junctions".into());
    }
    let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_dir != before.is_dir() || (!is_dir && !before.is_file()) {
        return Err("path changed while it was being validated".into());
    }

    let opened_path = normalized(
        final_path(&file)?
            .to_str()
            .ok_or_else(|| "opened path is not valid Unicode".to_owned())?,
    );
    let expected_path = normalized(
        canonical_path
            .to_str()
            .ok_or_else(|| "validated path is not valid Unicode".to_owned())?,
    );
    if opened_path != expected_path {
        return Err("opened path changed while it was being validated".into());
    }
    let within_indexed_root = indexed_roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| {
            root.to_str().is_some_and(|root| {
                opened_path.starts_with(&normalized(root))
                    && (opened_path.len() == normalized(root).len()
                        || opened_path.as_bytes().get(normalized(root).len()) == Some(&b'\\'))
            })
        });
    if !within_indexed_root {
        return Err("opened path is outside every indexed root".into());
    }
    // The handle deliberately denies FILE_SHARE_DELETE and remains alive in the caller
    // until the child exits, so the validated directory cannot be renamed or removed.
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_validated_index_path(
    _canonical_path: &std::path::Path,
    _indexed_roots: &[std::path::PathBuf],
) -> Result<std::fs::File, String> {
    Err("safe trigger_index directory handles are unsupported on this non-Unix, non-Windows platform".into())
}

/// Argument vector for the `indexa index` child. The `--` ends option parsing, so whatever
/// follows is a path even if it looks like a flag (defence in depth behind
/// [`validate_index_path`]).
fn index_args(path: &str) -> [&str; 3] {
    ["index", "--", path]
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
            // Per-basis reconciliation: `bytes_served` measures different things across
            // surfaces (this MCP server records the full rendered response; web/CLI `ask`
            // record answer+citations), so the aggregate above blends them. Mirrors
            // `indexa status`'s convention (apps/indexa/src/commands/status.rs) — only
            // show the split once more than one basis actually contributed, otherwise
            // the aggregate line already tells the whole story.
            if let Ok(by_basis) = store.usage_by_basis(indexa_core::store::USAGE_WEEK_SECS) {
                if by_basis.len() > 1 {
                    out.push_str("\nBy served basis:");
                    for (basis, u) in &by_basis {
                        let saved = u.bytes_counterfactual.saturating_sub(u.bytes_served) / 4;
                        out.push_str(&format!(
                            "\n  {basis}: {} call{} · ~{saved} tokens saved",
                            u.calls,
                            if u.calls == 1 { "" } else { "s" },
                        ));
                    }
                }
            }
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
             audio_transcribe={}, video_caption={}, video_transcribe={}",
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
            c.parsers.video.transcribe,
        );
        Ok(ok_text(out))
    }

    /// Garbage-collect orphaned rows (chunks/summaries left behind after a root was removed).
    #[tool(
        description = "Garbage-collect orphaned index rows — chunks and summaries left behind after their files/roots were removed. Returns how many rows were pruned. Safe: only removes rows with no matching entry.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    pub(crate) async fn prune(&self) -> Result<CallToolResult, ErrorData> {
        let mut store = self.store()?;
        let counts = store.prune_orphans().map_err(mcp_err)?;
        Ok(ok_text(format!("Pruned {}.", counts.summary())))
    }

    /// Trigger a full scan → deep-index → summarize pipeline on a path.
    #[tool(
        description = "Start an `indexa index <path>` run for a path inside an already-indexed root: scan files, compute embeddings, \
                       and generate summaries. Runs as a background subprocess and returns \
                       when indexing is complete. Use before asking questions about new or \
                       changed files.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub(crate) async fn trigger_index(
        &self,
        params: Parameters<TriggerIndexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = params.0.path;
        // `path` comes from the MCP caller (often a model that has read untrusted text) and is
        // handed to a CLI whose parser accepts global flags like `--config=<file>` anywhere.
        let roots = self
            .store()?
            .root_paths()
            .map_err(mcp_err)?
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>();
        let canonical_path = validate_index_path(&path, &roots).map_err(mcp_invalid)?;
        let _validated_handle =
            open_validated_index_path(&canonical_path, &roots).map_err(mcp_invalid)?;
        #[cfg(unix)]
        let (expected_dev, expected_ino) = {
            use std::os::unix::fs::MetadataExt;
            let metadata = _validated_handle.metadata().map_err(mcp_err)?;
            (metadata.dev().to_string(), metadata.ino().to_string())
        };
        let roots_json = serde_json::to_string(&roots).map_err(mcp_err)?;
        // Spawn `indexa index <path>` as a subprocess, using THIS server's own binary (not a
        // bare PATH lookup) so we re-invoke the exact `indexa` serving this MCP session. Both
        // processes open the same DB via WAL + 5s busy_timeout, which handles contention safely.
        let mut child = tokio::process::Command::new(indexa_exe()?);
        child
            .args(index_args(
                canonical_path
                    .to_str()
                    .ok_or_else(|| mcp_invalid("path is not valid UTF-8"))?,
            ))
            .env("INDEXA_TRIGGER_INDEX_ROOTS", roots_json);
        #[cfg(unix)]
        child
            .env("INDEXA_TRIGGER_INDEX_DEV", expected_dev)
            .env("INDEXA_TRIGGER_INDEX_INO", expected_ino);
        let output = run_trigger_child(&mut child, trigger_index_timeout()).await?;

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
                       idempotent. Notes are redacted on `export_pack` like any other file. \
                       Set `anchor` to a code path or bare symbol name to have \
                       `dependencies`/`blast_radius`/`symbol_context` surface this note inline \
                       whenever that path/symbol comes up later.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    pub(crate) async fn add_note(
        &self,
        params: Parameters<AddNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let AddNoteParams {
            pack,
            title,
            body,
            anchor,
        } = params.0;

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
        let note_path = indexa_core::notes::write_note_file_anchored(
            data_dir,
            &pack,
            &title,
            &body,
            anchor.as_deref(),
        )
        .map_err(|e| mcp_err(format!("writing note: {e}")))?;

        let note_path_str = note_path.to_string_lossy().into_owned();

        // Register the note in the pack so `search_pack` / `export_pack` find it.
        store
            .add_pack_paths(&pack_rec.id, std::slice::from_ref(&note_path_str))
            .map_err(mcp_err)?;

        if let Some(a) = &anchor {
            let kind = if a.starts_with('/') { "path" } else { "symbol" };
            store
                .upsert_note_anchor(&note_path_str, a, kind, &title, &pack)
                .map_err(mcp_err)?;
        }

        // Index immediately: scan + deep-embed + summarize the notes directory so the
        // note is searchable right away. Best-effort — a failure here still means the
        // note is written and pack-registered; the caller can trigger re-indexing later.
        let notes_dir = data_dir.join("notes");
        let notes_dir_str = notes_dir.to_string_lossy().into_owned();
        let index_result = tokio::process::Command::new(indexa_exe()?)
            .args(index_args(&notes_dir_str))
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

        let anchor_note = match &anchor {
            Some(a) => format!("\nAnchored to: {a}"),
            None => String::new(),
        };
        Ok(ok_text(format!(
            "Note \"{title}\" added to pack \"{pack}\" and indexed.\nFile: {note_path_str}{anchor_note}{index_note}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::indexa_exe;
    use crate::IndexaMcp;
    use indexa_core::config::Config;
    use indexa_core::store::Store;
    use indexa_embed::Embedder;
    use indexa_llm::Generator;
    use rmcp::model::CallToolResult;
    use std::sync::Arc;

    #[cfg(windows)]
    #[tokio::test]
    async fn assignment_failure_terminates_suspended_trigger_child() {
        use std::os::windows::process::CommandExt;
        let marker =
            std::env::temp_dir().join(format!("idx-mcp-never-ran-{}.txt", std::process::id()));
        let mut command = tokio::process::Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-Command"])
            .arg(format!("Set-Content '{}' ran", marker.display()));
        command
            .as_std_mut()
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        let mut child = command.spawn().unwrap();
        let error = super::assign_or_terminate_child(&mut child, |_| {
            Err(std::io::Error::other("simulated assignment failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("simulated"));
        let _ = child.wait().await.unwrap();
        assert!(
            !marker.exists(),
            "suspended trigger child ran after assignment failure"
        );
    }

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

    struct StubEmbedder;
    #[async_trait::async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }
    struct StubGenerator;
    #[async_trait::async_trait]
    impl Generator for StubGenerator {
        async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok("stub".to_owned())
        }
    }

    fn tool_text(r: CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `get_stats` must reconcile bases the same way `indexa status` does: silent when
    /// only one basis contributed (the aggregate savings line already says it all), and
    /// a per-basis breakdown once more than one basis is present — using
    /// [`indexa_core::store::Store::usage_by_basis`], not a fresh aggregation.
    #[tokio::test]
    async fn get_stats_shows_by_basis_split_only_when_more_than_one_basis_present() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .record_tool_usage_with_basis(
                    "mcp",
                    "search",
                    100,
                    1_000,
                    None,
                    "rendered_response",
                )
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            dbpath.clone(),
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let stats = tool_text(mcp.get_stats().await.unwrap());
        assert!(
            !stats.contains("By served basis"),
            "a single basis must not trigger the split, got: {stats}"
        );

        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .record_tool_usage_with_basis("web", "ask", 50, 500, None, "answer_and_citations")
                .unwrap();
        }
        let stats = tool_text(mcp.get_stats().await.unwrap());
        assert!(
            stats.contains("By served basis"),
            "two distinct bases must trigger the split, got: {stats}"
        );
        assert!(
            stats.contains("rendered_response") && stats.contains("answer_and_citations"),
            "the split must name both contributing bases, got: {stats}"
        );
    }

    /// The `prune` tool must report the note-anchor and co-change rows the sweep deletes
    /// (F15 added them to `prune_orphans`; the message kept printing only the older six
    /// counts, so those deletions were invisible to the caller).
    #[tokio::test]
    async fn prune_reports_note_anchor_and_co_change_counts() {
        use indexa_core::walker::{Entry, EntryKind};
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            // One live entry, so prune's empty-index guard does not short-circuit.
            store
                .upsert_entries(&[Entry {
                    path: "/p/live.md".into(),
                    kind: EntryKind::File,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
            store
                .upsert_note_anchor("/p/gone-note.md", "/p/live.md", "path", "t", "pack")
                .unwrap();
            store
                .replace_co_change(&[("/p/live.md".into(), "/p/gone.md".into(), 2)])
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            dbpath.clone(),
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let out = tool_text(mcp.prune().await.unwrap());
        assert!(
            out.contains("1 note anchor(s)") && out.contains("1 co-change pair(s)"),
            "prune must report the swept note-anchor/co-change rows, got: {out}"
        );
        let store = Store::open(&dbpath).unwrap();
        assert!(store.count_orphans().unwrap().is_empty());
    }

    /// H2: `trigger_index` must not let its `path` become an `indexa index` flag. The
    /// argument vector ends option parsing before the path, and an option-shaped, relative or
    /// missing path is refused before any child is spawned.
    #[test]
    fn trigger_index_path_cannot_become_a_flag() {
        use super::{index_args, validate_index_path};
        assert_eq!(
            index_args("--config=/tmp/x.toml"),
            ["index", "--", "--config=/tmp/x.toml"]
        );
        let roots = Vec::new();
        assert!(validate_index_path("--config=/tmp/x.toml", &roots).is_err());
        assert!(validate_index_path("-h", &roots).is_err());
        assert!(validate_index_path("", &roots).is_err());
        assert!(validate_index_path("relative/dir", &roots).is_err());
        assert!(validate_index_path("/definitely/not/a/real/dir-xyz", &roots).is_err());
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate_index_path(tmp.path().to_str().unwrap(), &roots).is_err());
        assert!(
            validate_index_path(tmp.path().to_str().unwrap(), &[tmp.path().to_owned()]).is_ok()
        );
        let outside = tempfile::tempdir().unwrap();
        assert!(
            validate_index_path(outside.path().to_str().unwrap(), &[tmp.path().to_owned()])
                .is_err()
        );
        #[cfg(unix)]
        {
            let link = tmp.path().join("outside-link");
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            assert!(validate_index_path(link.to_str().unwrap(), &[tmp.path().to_owned()]).is_err());
        }
        let traversal = tmp
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap());
        assert!(
            validate_index_path(traversal.to_str().unwrap(), &[tmp.path().to_owned()]).is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn trigger_index_rejects_directory_replaced_by_junction_after_validation() {
        use super::{open_validated_index_path, validate_index_path};
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let indexed = tmp.path().join("indexed");
        let target = indexed.join("target");
        let moved = indexed.join("moved-target");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let roots = vec![indexed.clone()];
        let canonical = validate_index_path(target.to_str().unwrap(), &roots).unwrap();

        std::fs::rename(&target, &moved).unwrap();
        let junction = Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&target)
            .arg(&outside)
            .output()
            .expect("Windows runner provides cmd.exe");
        assert!(
            junction.status.success(),
            "could not create junction for the reparse-point race test: {}",
            String::from_utf8_lossy(&junction.stderr)
        );

        let result = open_validated_index_path(&canonical, &roots);
        assert!(
            result.is_err(),
            "trigger_index accepted a junction swapped in after validation"
        );
    }

    #[cfg(windows)]
    #[test]
    fn trigger_index_holds_a_validated_directory_for_indexing() {
        use super::{open_validated_index_path, validate_index_path};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("indexed");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("marker.txt"), "indexed content").unwrap();
        let roots = vec![root.clone()];
        let canonical = validate_index_path(root.to_str().unwrap(), &roots).unwrap();
        let handle = open_validated_index_path(&canonical, &roots)
            .expect("a normal indexed directory remains supported on Windows");

        assert!(handle.metadata().unwrap().is_dir());
        let moved = tmp.path().join("moved-indexed");
        assert!(
            std::fs::rename(&root, &moved).is_err(),
            "the held directory handle must deny rename/delete while indexing"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("marker.txt")).unwrap(),
            "indexed content"
        );
    }

    #[tokio::test]
    async fn trigger_index_refuses_an_option_shaped_path_before_spawning() {
        use crate::TriggerIndexParams;
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = IndexaMcp::new(
            dbdir.path().join("idx.db"),
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let err = mcp
            .trigger_index(rmcp::handler::server::wrapper::Parameters(
                TriggerIndexParams {
                    path: "--config=/tmp/x.toml".to_owned(),
                },
            ))
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("invalid path"),
            "expected a validation refusal, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn trigger_index_uses_roots_from_the_open_index() {
        use crate::TriggerIndexParams;
        use indexa_core::walker::{Entry, EntryKind};
        use rmcp::handler::server::wrapper::Parameters;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("indexed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let db = tmp.path().join("idx.db");
        {
            let mut store = Store::open(&db).unwrap();
            store
                .upsert_entries(&[Entry {
                    path: root,
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            db,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let err = mcp
            .trigger_index(Parameters(TriggerIndexParams {
                path: outside.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("outside every indexed root"),
            "unexpected trigger_index result: {err:?}"
        );
    }

    /// H1: `changed_impact` must refuse an option-shaped `scope` (`--output=<path>` makes
    /// `git diff` overwrite that file) and a `root` outside every indexed root — and must not
    /// reach git in either case.
    #[cfg(unix)]
    #[tokio::test]
    async fn changed_impact_refuses_option_scope_and_unindexed_root() {
        use crate::ChangedImpactParams;
        use indexa_core::walker::{Entry, EntryKind};
        use rmcp::handler::server::wrapper::Parameters;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        for dir in [&repo, &outside] {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["init", "-q"])
                .status()
                .unwrap();
            assert!(st.success());
        }
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();

        let dbpath = tmp.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_entries(&[Entry {
                    path: repo.clone(),
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let params = |root: &std::path::Path, scope: String| {
            Parameters(ChangedImpactParams {
                root: Some(root.to_string_lossy().into_owned()),
                scope: Some(scope),
                strict: false,
                depth: None,
                include_heritage: false,
            })
        };

        let err = mcp
            .changed_impact(params(&repo, format!("--output={}", victim.display())))
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("must not start with '-'"),
            "expected the option-shaped scope to be refused, got: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "precious",
            "git must never have been run with the injected --output"
        );

        let err = mcp
            .changed_impact(params(&outside, "unstaged".to_owned()))
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("not within an indexed root"),
            "expected the unindexed root to be refused, got: {err:?}"
        );

        // The legitimate call still works (clean repo ⇒ no changes).
        let ok = tool_text(
            mcp.changed_impact(params(&repo, "unstaged".to_owned()))
                .await
                .unwrap(),
        );
        assert!(ok.contains("No changes found"), "got: {ok}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_trigger_child_is_killed() {
        let dir = std::env::temp_dir().join(format!(
            "indexa-trigger-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let mut child = tokio::process::Command::new("sh");
        child
            .arg("-c")
            .arg(format!("echo $$ > {}; exec sleep 30", pid_file.display()));
        let error = super::run_trigger_child(&mut child, std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("timed out"));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_trigger_child_kills_its_grandchild() {
        let dir = std::env::temp_dir().join(format!(
            "indexa-trigger-group-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let parent_file = dir.join("parent");
        let child_file = dir.join("grandchild");
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(format!(
            "echo $$ > {}; sleep 30 & echo $! > {}; wait",
            parent_file.display(),
            child_file.display()
        ));
        let result =
            super::run_trigger_child(&mut command, std::time::Duration::from_millis(150)).await;
        assert!(result.is_err());
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        for pid_file in [&parent_file, &child_file] {
            let pid = std::fs::read_to_string(pid_file).unwrap().trim().to_owned();
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "process {pid} survived group timeout"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timed_out_trigger_child_kills_child_and_grandchild_job() {
        let dir = std::env::temp_dir().join(format!(
            "indexa-trigger-job-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let parent_file = dir.join("parent.pid");
        let grandchild_file = dir.join("grandchild.pid");
        let quote_path = |path: &std::path::Path| path.to_string_lossy().replace('\'', "''");
        let script = format!(
            "$PID | Set-Content -NoNewline '{}'; $p = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 30') -PassThru; $p.Id | Set-Content -NoNewline '{}'; Start-Sleep -Seconds 30",
            quote_path(&parent_file),
            quote_path(&grandchild_file)
        );
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", &script]);
        let result =
            super::run_trigger_child(&mut command, std::time::Duration::from_secs(5)).await;
        assert!(result.is_err(), "trigger child unexpectedly completed");
        let parent = wait_for_pid(&parent_file).await;
        let grandchild = wait_for_pid(&grandchild_file).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !is_running(parent),
            "child process {parent} survived timeout"
        );
        assert!(
            !is_running(grandchild),
            "grandchild process {grandchild} survived timeout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn immediate_trigger_grandchild_is_killed_by_timeout_job() {
        let dir =
            std::env::temp_dir().join(format!("indexa-trigger-immediate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let grandchild_file = dir.join("grandchild.pid");
        let script = format!("$p = Start-Process powershell.exe -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 30') -PassThru; $p.Id | Set-Content -NoNewline '{}' ; Start-Sleep -Seconds 30", grandchild_file.display());
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", &script]);
        let result =
            super::run_trigger_child(&mut command, std::time::Duration::from_secs(5)).await;
        assert!(result.is_err(), "trigger child unexpectedly completed");
        let grandchild = wait_for_pid(&grandchild_file).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !is_running(grandchild),
            "immediate grandchild {grandchild} survived timeout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    async fn wait_for_pid(path: &std::path::Path) -> u32 {
        for _ in 0..100 {
            if let Ok(value) = std::fs::read_to_string(path) {
                if let Ok(pid) = value.parse() {
                    return pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("process did not record pid in {}", path.display());
    }

    #[cfg(windows)]
    fn is_running(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe { CloseHandle(handle) };
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
}
