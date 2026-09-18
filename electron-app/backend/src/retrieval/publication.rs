use super::{RetrievalError, RetrievalResult};
use crate::runtime::Cancellation;
use std::path::Path;

// The caller closes SQLite and syncs the staged file before publication. Pin
// both directories, then perform one no-clobber rename after the final seal.
// Unlike link/unlink, this never publishes a two-link file rejected on reopen.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn publish_no_replace(
    source: &Path,
    target: &Path,
    cancellation: &Cancellation,
) -> RetrievalResult<()> {
    use std::{
        ffi::CString,
        fs::OpenOptions,
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let open_dir = |path: &Path| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
    };
    let from = open_dir(source.parent().ok_or(RetrievalError::InvalidPath)?)?;
    let to = open_dir(target.parent().ok_or(RetrievalError::InvalidPath)?)?;
    let source_name = CString::new(
        source
            .file_name()
            .ok_or(RetrievalError::InvalidPath)?
            .as_bytes(),
    )
    .map_err(|_| RetrievalError::InvalidPath)?;
    let target_name = CString::new(
        target
            .file_name()
            .ok_or(RetrievalError::InvalidPath)?
            .as_bytes(),
    )
    .map_err(|_| RetrievalError::InvalidPath)?;
    let _permit = super::types::seal(cancellation)?;
    // SAFETY: owned directory descriptors and NUL-terminated names stay alive.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source_name.as_ptr(),
            to.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source_name.as_ptr(),
            to.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists {
                RetrievalError::Conflict
            } else {
                RetrievalError::Storage
            },
        );
    }
    to.sync_all()?;
    from.sync_all()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn publish_no_replace(_: &Path, _: &Path, _: &Cancellation) -> RetrievalResult<()> {
    // Fail closed rather than silently falling back to a crash-unsafe operation.
    Err(RetrievalError::Storage)
}
