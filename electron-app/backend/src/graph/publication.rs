use super::{GraphError, GraphResult};
use crate::{protocol::ErrorCode, runtime::Cancellation};
use std::path::Path;

// Same invariant as history publication: no two-hardlink crash window and no
// clobber, with pinned directory descriptors and durable directory publication.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn publish_no_replace(
    source: &Path,
    target: &Path,
    c: &Cancellation,
) -> GraphResult<()> {
    use std::{
        ffi::CString,
        fs::OpenOptions,
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let open_dir = |p: &Path| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(p)
            .map_err(|_| GraphError::InvalidPath)
    };
    let from = open_dir(source.parent().ok_or(GraphError::InvalidPath)?)?;
    let to = open_dir(target.parent().ok_or(GraphError::InvalidPath)?)?;
    let name = |p: &Path| {
        CString::new(p.file_name().ok_or(GraphError::InvalidPath)?.as_bytes())
            .map_err(|_| GraphError::InvalidPath)
    };
    let source_name = name(source)?;
    let target_name = name(target)?;
    let _permit = c.begin_commit().map_err(|e| {
        if e == ErrorCode::Cancelled {
            GraphError::Cancelled
        } else {
            GraphError::AlreadySealed
        }
    })?;
    // SAFETY: owned directory descriptors and NUL-terminated names remain live.
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
        return Err(GraphError::Storage);
    }
    to.sync_all().map_err(|_| GraphError::Storage)?;
    from.sync_all().map_err(|_| GraphError::Storage)?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn publish_no_replace(_: &Path, _: &Path, _: &Cancellation) -> GraphResult<()> {
    Err(GraphError::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CozoGraphStore;

    #[tokio::test]
    async fn graph_published_database_reopens_before_any_staging_cleanup() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("stage.sqlite3");
        let target = root.path().join("graph.sqlite3");
        let store =
            CozoGraphStore::open(root.path().into(), source.clone(), Cancellation::default())
                .await
                .unwrap();
        store.close().await.unwrap();
        let c = Cancellation::default();
        publish_no_replace(&source, &target, &c).unwrap();
        // No cleanup/destructor between publication and reopen. This is the
        // exact point at which a link/unlink implementation was crash-unsafe.
        assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 1);
        assert!(!source.exists());
        assert!(!c.cancel());
        let reopened = CozoGraphStore::open(root.path().into(), target, Cancellation::default())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .status(Cancellation::default())
                .await
                .unwrap()
                .papers,
            0
        );
    }

    #[test]
    fn graph_publication_collision_and_prepublication_cancel_preserve_files() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("stage");
        let target = root.path().join("target");
        std::fs::write(&source, "stage").unwrap();
        let c = Cancellation::default();
        c.cancel();
        assert_eq!(
            publish_no_replace(&source, &target, &c),
            Err(GraphError::Cancelled)
        );
        assert!(!target.exists());
        std::fs::write(&target, "existing").unwrap();
        let c = Cancellation::default();
        assert_eq!(
            publish_no_replace(&source, &target, &c),
            Err(GraphError::Storage)
        );
        assert!(!c.cancel());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "existing");
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "stage");
    }
}
