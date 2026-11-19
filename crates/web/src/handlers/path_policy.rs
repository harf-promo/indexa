//! Path confinement shared by web job and watch handlers.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    fn capture(path: PathBuf) -> std::io::Result<Self> {
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
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            identity,
            _handle: Arc::new(handle),
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.path
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
}
