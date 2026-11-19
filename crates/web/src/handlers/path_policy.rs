//! Path confinement shared by web job and watch handlers.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(all(test, unix))]
static OPENED_VALIDATED_PATHS: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

#[cfg(all(test, unix))]
static OPENED_VALIDATED_PATHS_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(all(test, unix))]
pub(crate) fn lock_opened_validated_path_recording() -> std::sync::MutexGuard<'static, ()> {
    OPENED_VALIDATED_PATHS_TEST_LOCK
        .lock()
        .expect("opened-path recorder test lock poisoned")
}

#[cfg(all(test, unix))]
pub(crate) fn begin_opened_validated_path_recording() {
    *OPENED_VALIDATED_PATHS
        .lock()
        .expect("opened-path recorder poisoned") = Some(Vec::new());
}

#[cfg(all(test, unix))]
pub(crate) fn take_opened_validated_paths() -> Vec<PathBuf> {
    OPENED_VALIDATED_PATHS
        .lock()
        .expect("opened-path recorder poisoned")
        .take()
        .unwrap_or_default()
}

use crate::{dto::err_json, AppState};

pub(crate) struct IndexedPathError {
    status: StatusCode,
    pub(crate) message: String,
}

/// A canonical indexed path together with the identity checked during confinement.
/// Call `revalidate` at the point where a background operation starts using it; this
/// rejects a directory that was replaced after request validation.
#[derive(Clone, Debug)]
pub(crate) struct ValidatedIndexedPath {
    path: String,
    real_path: PathBuf,
    identity: FileIdentity,
    _handle: Arc<File>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: u64,
}

impl FileIdentity {
    fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
            use windows_sys::Win32::Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            let ok = unsafe {
                GetFileInformationByHandle(
                    file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                    &mut info,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = metadata;
            Ok(Self {
                volume: info.dwVolumeSerialNumber,
                index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
            Ok(Self {})
        }
    }

    fn of_file(file: &File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            };
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            let ok = unsafe {
                GetFileInformationByHandle(
                    file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                    &mut info,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = metadata;
            Ok(Self {
                volume: info.dwVolumeSerialNumber,
                index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
            Ok(Self {})
        }
    }
}

impl ValidatedIndexedPath {
    pub(crate) fn capture(path: PathBuf) -> std::io::Result<Self> {
        let path = std::fs::canonicalize(path)?;
        #[cfg(unix)]
        let handle = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&path)?
        };
        #[cfg(windows)]
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
                FILE_SHARE_WRITE,
            };
            std::fs::OpenOptions::new()
                .read(true)
                // Keep the validated target from being renamed/replaced while the job or
                // watcher uses it. Omitting FILE_SHARE_DELETE is intentional.
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)?
        };
        #[cfg(not(any(unix, windows)))]
        let handle = File::open(&path)?;

        let identity = FileIdentity::of_file(&handle)?;
        if identity != FileIdentity::of(&path)? {
            return Err(std::io::Error::other(
                "path changed while opening validated target",
            ));
        }
        let real_path = opened_handle_path(&handle)?;
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            real_path,
            identity,
            _handle: Arc::new(handle),
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.path
    }

    fn ensure_root_identity(&self) -> std::io::Result<()> {
        let path_identity = FileIdentity::of(Path::new(&self.path))?;
        let handle_identity = FileIdentity::of_file(&self._handle)?;
        if path_identity != self.identity || handle_identity != self.identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "indexed root identity changed",
            ));
        }
        Ok(())
    }

    /// Open a file without following a final symlink, then verify the opened object's real path
    /// is component-wise beneath this indexed root. Callers must read from the returned handle;
    /// reopening `path` would reintroduce the check/use race.
    pub(crate) fn open_file(&self, path: &Path) -> std::io::Result<File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            options
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(path)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "reparse points are not indexed",
                ));
            }
        }
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        self.ensure_root_identity()?;
        let real = opened_handle_path(&file)?;
        #[cfg(all(test, unix))]
        if let Some(paths) = OPENED_VALIDATED_PATHS
            .lock()
            .expect("opened-path recorder poisoned")
            .as_mut()
        {
            paths.push(real.clone());
        }
        if !real.starts_with(&self.real_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "opened file is outside indexed root",
            ));
        }
        Ok(file)
    }

    /// Copy a validated open handle into a private temporary directory for path-based parsers.
    /// The original file is read only through the checked handle, and the returned directory
    /// keeps the snapshot alive for any parser or secondary media pass that opens its path.
    pub(crate) fn snapshot_file(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> std::io::Result<(
        tempfile::TempDir,
        PathBuf,
        u64,
        Option<std::time::SystemTime>,
    )> {
        use std::io::Write;
        let mut source = self.open_file(path)?;
        let metadata = source.metadata()?;
        if max_bytes > 0 && metadata.len() > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("file exceeds {max_bytes}-byte snapshot limit"),
            ));
        }
        let dir = tempfile::tempdir()?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no name")
        })?;
        let snapshot = dir.path().join(name);
        let mut dest = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&snapshot)?;
        std::io::copy(&mut source, &mut dest)?;
        dest.flush()?;
        Ok((dir, snapshot, metadata.len(), metadata.modified().ok()))
    }

    /// Open and validate a directory identity before permitting it to contribute entries.
    pub(crate) fn directory_is_beneath_root(&self, path: &Path) -> bool {
        let result: std::io::Result<bool> = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                use windows_sys::Win32::Storage::FileSystem::{
                    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
                    FILE_SHARE_WRITE,
                };
                options
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
            }
            let file = options.open(path)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Ok(false);
                }
            }
            if !file.metadata()?.is_dir() {
                return Ok(false);
            }
            self.ensure_root_identity()?;
            Ok(opened_handle_path(&file)?.starts_with(&self.real_path))
        })();
        result.unwrap_or(false)
    }

    pub(crate) fn revalidate(&self) -> Result<(), IndexedPathError> {
        let path = Path::new(&self.path);
        let canonical = std::fs::canonicalize(path).map_err(|_| {
            IndexedPathError::new(StatusCode::FORBIDDEN, "path changed after validation")
        })?;
        let identity = FileIdentity::of(path).map_err(|_| {
            IndexedPathError::new(StatusCode::FORBIDDEN, "path changed after validation")
        })?;
        let handle_identity = FileIdentity::of_file(&self._handle).map_err(|_| {
            IndexedPathError::new(
                StatusCode::FORBIDDEN,
                "validated handle is no longer usable",
            )
        })?;
        if canonical != path || identity != self.identity || handle_identity != self.identity {
            return Err(IndexedPathError::new(
                StatusCode::FORBIDDEN,
                "path changed after validation",
            ));
        }
        Ok(())
    }
}

/// Drop entries whose opened file or directory identity no longer resolves beneath the validated
/// root. The core walker runs with binary sniffing disabled for validated roots; this function
/// performs that optional read from the already-verified handle.
pub(crate) fn confine_walk_entries(
    entries: Vec<indexa_core::walker::Entry>,
    validated: &ValidatedIndexedPath,
    sniff_binary: bool,
) -> Vec<indexa_core::walker::Entry> {
    use indexa_core::walker::EntryKind;
    use std::io::Read;
    entries
        .into_iter()
        .filter_map(|mut entry| {
            if entry.kind == EntryKind::Dir {
                if !validated.directory_is_beneath_root(&entry.path) {
                    tracing::warn!(path = %entry.path.display(), "skipping walked directory outside indexed root");
                    return None;
                }
                return Some(entry);
            }
            let mut file = match validated.open_file(&entry.path) {
                Ok(file) => file,
                Err(error) => {
                    tracing::warn!(path = %entry.path.display(), %error, "skipping walked file outside indexed root or unavailable");
                    return None;
                }
            };
            if sniff_binary {
                let mut sample = [0u8; 8192];
                match file.read(&mut sample) {
                    Ok(n) => entry.is_binary = indexa_core::text::is_binary(&sample[..n]),
                    Err(error) => tracing::warn!(path = %entry.path.display(), %error, "binary sniff failed"),
                }
            }
            Some(entry)
        })
        .collect()
}

/// Entry filter for a validated job walk. The ignore crate normally opens nested ignore files
/// inside its directory-read operation, before a visitor can inspect the directory handle. Safe
/// walks disable that automatic loading and build one matcher per directory from checked handle
/// snapshots here, before the walk queues that directory's children.
pub(crate) struct ValidatedWalkFilter {
    validated: ValidatedIndexedPath,
    root: PathBuf,
    cfg: indexa_core::walker::WalkConfig,
    matchers: Mutex<HashMap<PathBuf, ignore::gitignore::Gitignore>>,
}

impl ValidatedWalkFilter {
    pub(crate) fn new(
        validated: ValidatedIndexedPath,
        cfg: indexa_core::walker::WalkConfig,
    ) -> Self {
        Self {
            root: PathBuf::from(validated.as_str()),
            validated,
            cfg,
            matchers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn allows(&self, path: &Path, is_dir: bool) -> bool {
        if is_dir {
            if !self.validated.directory_is_beneath_root(path) {
                tracing::warn!(path = %path.display(), "skipping walked directory outside indexed root");
                return false;
            }
        } else if self.validated.open_file(path).is_err() {
            tracing::warn!(path = %path.display(), "skipping walked file outside indexed root");
            return false;
        }

        // Walkers may preserve an alias such as macOS /var while the captured root and opened
        // handles use /private/var. Normalize after handle validation and use one canonical path
        // for ancestry, matcher lookup, and relative-path matching.
        let path = match std::fs::canonicalize(path) {
            Ok(path) if path.starts_with(&self.root) => path,
            _ => {
                tracing::warn!(path = %path.display(), "skipping path outside canonical indexed root");
                return false;
            }
        };

        if !is_dir
            && path.file_name().is_some_and(|name| {
                [
                    ".gitignore",
                    ".ignore",
                    indexa_core::walker::CUSTOM_IGNORE_FILENAME,
                ]
                .iter()
                .any(|ignore_name| name == std::ffi::OsStr::new(ignore_name))
            })
        {
            return false;
        }

        let mut directories = Vec::new();
        let mut current = if is_dir {
            Some(path.as_path())
        } else {
            path.parent()
        };
        while let Some(dir) = current {
            if !dir.starts_with(&self.root) {
                break;
            }
            directories.push(dir.to_path_buf());
            if dir == self.root {
                break;
            }
            current = dir.parent();
        }
        directories.reverse();

        let mut ignored = false;
        {
            let matchers = match self.matchers.lock() {
                Ok(matchers) => matchers,
                Err(_) => return false,
            };
            for dir in &directories {
                let Some(matcher) = matchers.get(dir) else {
                    continue;
                };
                let Ok(relative) = path.strip_prefix(dir) else {
                    continue;
                };
                let matched = matcher.matched_path_or_any_parents(
                    relative,
                    is_dir || dir.as_path() != path.as_path(),
                );
                if !matched.is_none() {
                    ignored = matched.is_ignore();
                }
            }
        }
        if ignored {
            return false;
        }

        if is_dir {
            let Some(matcher) = self.matcher_for_directory(&path) else {
                tracing::warn!(path = %path.display(), "skipping directory because its ignore policy could not be checked safely");
                return false;
            };
            if let Ok(mut matchers) = self.matchers.lock() {
                matchers.insert(path.clone(), matcher);
            } else {
                return false;
            }
        }
        true
    }

    fn matcher_for_directory(&self, dir: &Path) -> Option<ignore::gitignore::Gitignore> {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        if dir == self.root {
            for pattern in &self.cfg.ignore {
                let _ = builder.add_line(None, pattern);
            }
        }
        let mut names = Vec::new();
        if self.cfg.respect_gitignore {
            names.extend([".gitignore", ".ignore"]);
            if dir == self.root {
                names.push(".git/info/exclude");
            }
            if self.cfg.custom_ignore {
                names.push(indexa_core::walker::CUSTOM_IGNORE_FILENAME);
            }
        }
        for name in names {
            let file_path = dir.join(name);
            let metadata = match std::fs::symlink_metadata(&file_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(path = %file_path.display(), %error, "could not inspect ignore file");
                    return None;
                }
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                tracing::warn!(path = %file_path.display(), "refusing non-regular or symlinked ignore file");
                return None;
            }
            let Ok((snapshot_dir, snapshot, _, _)) =
                self.validated.snapshot_file(&file_path, 1024 * 1024)
            else {
                tracing::warn!(path = %file_path.display(), "could not safely snapshot ignore file");
                return None;
            };
            let Ok(contents) = std::fs::read_to_string(&snapshot) else {
                tracing::warn!(path = %file_path.display(), "ignore file is not valid UTF-8");
                return None;
            };
            for line in contents.lines() {
                if let Err(error) = builder.add_line(Some(file_path.clone()), line) {
                    tracing::warn!(path = %file_path.display(), %error, "ignoring invalid ignore rule");
                }
            }
            drop(snapshot_dir);
        }
        builder.build().ok()
    }
}

/// Resolve the path represented by an already-open handle on supported CI platforms.
#[cfg(target_os = "linux")]
fn opened_handle_path(file: &File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "macos")]
fn opened_handle_path(file: &File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = [0i8; libc::PATH_MAX as usize];
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(target_os = "windows")]
fn opened_handle_path(file: &File) -> std::io::Result<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut buffer = vec![0u16; 32768];
    let len =
        unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if len == 0 || len as usize >= buffer.len() {
        return Err(std::io::Error::last_os_error());
    }
    buffer.truncate(len as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn opened_handle_path(_file: &File) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opened-handle path checks are unsupported",
    ))
}

impl IndexedPathError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for IndexedPathError {
    fn into_response(self) -> Response {
        err_json(self.status, self.message)
    }
}

/// Resolve a requested path and keep it within one of the indexed roots.
///
/// Returning the canonical path ensures downstream jobs/watchers use the resolved target rather
/// than re-resolving a caller-controlled `..` or symlink path after validation.
pub(crate) async fn canonical_indexed_path(
    state: &AppState,
    raw: &str,
) -> Result<ValidatedIndexedPath, IndexedPathError> {
    if raw.is_empty() || raw.len() > 4096 {
        return Err(IndexedPathError::new(
            StatusCode::BAD_REQUEST,
            "invalid or oversized path",
        ));
    }
    let canonical = std::fs::canonicalize(raw)
        .map_err(|_| IndexedPathError::new(StatusCode::NOT_FOUND, "path not found"))?;
    let roots =
        state.store.lock().await.root_paths().map_err(|e| {
            IndexedPathError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
        })?;
    let inside = roots
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(PathBuf::from(root)).ok())
        .any(|root| canonical.starts_with(root));
    if !inside {
        return Err(IndexedPathError::new(
            StatusCode::FORBIDDEN,
            "path is outside every indexed root",
        ));
    }
    ValidatedIndexedPath::capture(canonical)
        .map_err(|_| IndexedPathError::new(StatusCode::NOT_FOUND, "path changed while opening"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_directory_replaced_with_outside_symlink_after_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("indexed");
        let target = root.join("replace-me");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let validated = ValidatedIndexedPath::capture(target.clone()).unwrap();

        std::fs::rename(&target, root.join("moved")).unwrap();
        symlink(&outside, &target).unwrap();

        assert!(validated.revalidate().is_err());
    }

    #[test]
    fn rejects_opened_child_after_indexed_root_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("indexed");
        let moved = temp.path().join("indexed-original");
        std::fs::create_dir_all(&root).unwrap();
        let validated = ValidatedIndexedPath::capture(root.clone()).unwrap();

        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        let sentinel = root.join("secret.txt");
        std::fs::write(&sentinel, "replacement root content").unwrap();

        assert_eq!(
            validated.open_file(&sentinel).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!validated.directory_is_beneath_root(&root));
    }

    #[test]
    fn validated_walk_filter_loads_nested_ignore_rules_from_checked_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("real");
        let root_alias = temp.path().join("linked");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".gitignore"), "secret.txt\n").unwrap();
        let secret = nested.join("secret.txt");
        std::fs::write(&secret, "ignored content").unwrap();
        symlink(&root, &root_alias).unwrap();

        let validated = ValidatedIndexedPath::capture(root_alias.clone()).unwrap();
        let filter =
            ValidatedWalkFilter::new(validated, indexa_core::walker::WalkConfig::default());
        assert!(filter.allows(&root, true));
        assert!(filter.allows(&root_alias.join("nested"), true));
        assert!(!filter.allows(&root_alias.join("nested/secret.txt"), false));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn opened_file_outside_root_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("indexed");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("secret.txt");
        std::fs::write(&sentinel, "outside sentinel").unwrap();
        let validated = ValidatedIndexedPath::capture(root).unwrap();
        assert_eq!(
            validated.open_file(&sentinel).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
