//! Fail-closed native artifact publication. Only the fixed evaluation basename
//! can be written, under a trusted app-owned directory. No arbitrary path API.
//! macOS/Linux use no-follow descriptor traversal and advisory directory locking.
//! The app owns this namespace; an actor with the same UID/root can still defeat
//! advisory locks/rename permissions. Force-kill or lost output => outcome unknown.
use super::{
    EvaluationArtifact,
    contracts::{MAX_ARTIFACT_BYTES, bounded_result},
};
use crate::{
    config::StoragePaths, protocol::ErrorCode, registry::BackendResult, runtime::Cancellation,
};

pub const RESULTS_FILENAME: &str = "eval_results.json";

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        fs::{File, Metadata},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
        path::{Component, Path},
        sync::Arc,
    };

    struct Link {
        parent: File,
        name: CString,
        child: File,
    }
    struct Root {
        chain: Vec<Link>,
        directory: File,
    }
    #[derive(Clone)]
    pub struct ArtifactStore(Arc<Root>);
    pub(in crate::evaluations) struct Lease {
        store: ArtifactStore,
        directory: File,
        prior: Option<Identity>,
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            // Explicitly release before close: a concurrent fork can briefly inherit
            // this open-file description before CLOEXEC closes it in the child.
            // Merely closing our descriptor could leave a finished run locked.
            // SAFETY: this lease owns the live descriptor on which it took the lock.
            unsafe {
                libc::flock(self.directory.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
    #[derive(Clone, Debug, PartialEq)]
    struct Identity {
        dev: u64,
        ino: u64,
        size: u64,
        mtime: i64,
        mtime_ns: i64,
        ctime: i64,
        ctime_ns: i64,
    }
    fn identity(m: &Metadata) -> Identity {
        Identity {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        }
    }
    fn same(a: &File, b: &File) -> BackendResult<bool> {
        let a = a.metadata().map_err(io)?;
        let b = b.metadata().map_err(io)?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    fn io(_: std::io::Error) -> ErrorCode {
        ErrorCode::Internal
    }
    fn name(s: &std::ffi::OsStr) -> BackendResult<CString> {
        CString::new(s.as_bytes()).map_err(|_| ErrorCode::InvalidParams)
    }
    fn open(parent: &File, name: &CStr, flags: i32) -> std::io::Result<File> {
        // SAFETY: live parent, NUL-terminated component; mode is used only with O_CREAT.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fresh owned descriptor from openat.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn directory(parent: &File, name: &CStr) -> std::io::Result<File> {
        open(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)
    }
    fn owned_directory(file: &File) -> BackendResult<()> {
        let m = file.metadata().map_err(io)?;
        // SAFETY: geteuid has no arguments/preconditions.
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
            return Err(ErrorCode::InvalidParams);
        }
        Ok(())
    }
    fn components(path: &Path) -> BackendResult<Vec<CString>> {
        if !path.is_absolute() || path.as_os_str().as_bytes().windows(3).any(|s| s == b"/./") {
            return Err(ErrorCode::InvalidParams);
        }
        path.components()
            .filter(|c| !matches!(c, Component::RootDir))
            .map(|c| match c {
                Component::Normal(n) => name(n),
                _ => Err(ErrorCode::InvalidParams),
            })
            .collect()
    }
    impl ArtifactStore {
        pub fn from_paths(paths: &StoragePaths) -> BackendResult<Self> {
            let root = components(&paths.data_root)?;
            let all = components(&paths.evaluation_results)?;
            if root.is_empty()
                || all.len() <= root.len()
                || !paths.evaluation_results.starts_with(&paths.data_root)
            {
                return Err(ErrorCode::InvalidParams);
            }
            let target = paths.evaluation_results.join(RESULTS_FILENAME);
            for forbidden in [
                &paths.history_database,
                &paths.vector_database,
                &paths.graph_database,
                &paths.bm25_index,
                &paths.resources,
                &paths.papers,
            ] {
                components(forbidden)?;
                if target.starts_with(forbidden)
                    || forbidden.starts_with(&paths.evaluation_results)
                    || paths.data_root.starts_with(forbidden)
                {
                    return Err(ErrorCode::InvalidParams);
                }
            }
            let mut current = File::open("/").map_err(io)?;
            let mut chain = Vec::new();
            for (index, component) in all.into_iter().enumerate() {
                let child = match directory(&current, &component) {
                    Ok(child) => child,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::NotFound && index + 1 >= root.len() =>
                    {
                        // SAFETY: live parent, one validated component, private new directory.
                        let made = unsafe {
                            libc::mkdirat(current.as_raw_fd(), component.as_ptr(), 0o700)
                        };
                        if made != 0
                            && std::io::Error::last_os_error().kind()
                                != std::io::ErrorKind::AlreadyExists
                        {
                            return Err(ErrorCode::Internal);
                        }
                        directory(&current, &component).map_err(io)?
                    }
                    Err(_) => return Err(ErrorCode::InvalidParams),
                };
                if index + 1 >= root.len() {
                    owned_directory(&child)?;
                }
                let next = child.try_clone().map_err(io)?;
                chain.push(Link {
                    parent: current,
                    name: component,
                    child,
                });
                current = next;
            }
            let store = Self(Arc::new(Root {
                chain,
                directory: current,
            }));
            store.unchanged()?;
            Ok(store)
        }
        fn unchanged(&self) -> BackendResult<()> {
            for link in &self.0.chain {
                if !same(
                    &directory(&link.parent, &link.name).map_err(io)?,
                    &link.child,
                )? {
                    return Err(ErrorCode::InvalidParams);
                }
            }
            owned_directory(&self.0.directory)
        }
        pub(in crate::evaluations) fn acquire(&self) -> BackendResult<Lease> {
            self.unchanged()?;
            let dir = directory(&self.0.directory, c".").map_err(io)?;
            // SAFETY: independently opened live descriptor; nonblocking advisory lock.
            if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
                        ErrorCode::Busy
                    } else {
                        ErrorCode::Internal
                    },
                );
            }
            let mut lease = Lease {
                store: self.clone(),
                directory: dir,
                prior: None,
            };
            lease.prior = read_prior(&lease.directory)?;
            Ok(lease)
        }
        /// Synchronous; call only on a blocking-capable runner. No worker is spawned.
        pub fn publish(
            &self,
            artifact: &EvaluationArtifact,
            cancellation: &Cancellation,
        ) -> BackendResult<()> {
            cancellation.check()?;
            let lease = self.acquire()?;
            bounded_result(&artifact.complete())?;
            let bytes = artifact.serialize()?;
            lease.publish(&bytes, cancellation)
        }
    }
    fn read_prior(directory: &File) -> BackendResult<Option<Identity>> {
        let file = match open(directory, c"eval_results.json", libc::O_RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ErrorCode::InvalidParams),
        };
        let m = file.metadata().map_err(io)?;
        if !m.is_file() || m.nlink() != 1 || m.len() > MAX_ARTIFACT_BYTES as u64 {
            return Err(ErrorCode::InvalidParams);
        }
        let before = identity(&m);
        let mut text = String::new();
        (&file)
            .take(MAX_ARTIFACT_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(io)?;
        EvaluationArtifact::parse(&text)?;
        if identity(&file.metadata().map_err(io)?) != before {
            return Err(ErrorCode::InvalidParams);
        }
        Ok(Some(before))
    }
    struct Staging<'a> {
        directory: &'a File,
        name: CString,
        file: File,
        renamed: bool,
    }
    impl Drop for Staging<'_> {
        fn drop(&mut self) {
            if !self.renamed
                && open(self.directory, &self.name, libc::O_RDONLY)
                    .ok()
                    .is_some_and(|f| same(&f, &self.file).unwrap_or(false))
            {
                // SAFETY: remove only our verified staging inode relative to the pinned directory.
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Point {
        Staged,
        Synced,
        Sealed,
        Renamed,
    }
    impl Lease {
        pub(in crate::evaluations) fn publish(
            &self,
            bytes: &[u8],
            token: &Cancellation,
        ) -> BackendResult<()> {
            self.publish_inner(bytes, token, |_, _| Ok(()))
        }
        fn publish_inner(
            &self,
            bytes: &[u8],
            token: &Cancellation,
            mut at: impl FnMut(Point, &File) -> BackendResult<()>,
        ) -> BackendResult<()> {
            token.check()?;
            self.store.unchanged()?;
            let name = CString::new(format!(".eval_results.{}.tmp", uuid::Uuid::new_v4()))
                .map_err(|_| ErrorCode::Internal)?;
            let file = open(
                &self.directory,
                &name,
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            )
            .map_err(io)?;
            let mut staging = Staging {
                directory: &self.directory,
                name,
                file,
                renamed: false,
            };
            at(Point::Staged, &staging.file)?;
            staging.file.write_all(bytes).map_err(io)?;
            staging.file.sync_all().map_err(io)?;
            at(Point::Synced, &staging.file)?;
            token.check()?;
            self.store.unchanged()?;
            if read_prior(&self.directory)? != self.prior {
                return Err(ErrorCode::InvalidParams);
            }
            let staged = open(&self.directory, &staging.name, libc::O_RDONLY).map_err(io)?;
            if !same(&staged, &staging.file)? || staged.metadata().map_err(io)?.nlink() != 1 {
                return Err(ErrorCode::InvalidParams);
            }
            let _permit = token.begin_commit()?;
            at(Point::Sealed, &staging.file)?;
            // SAFETY: source and fixed target are relative to the same pinned directory.
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    staging.name.as_ptr(),
                    self.directory.as_raw_fd(),
                    c"eval_results.json".as_ptr(),
                )
            } != 0
            {
                return Err(ErrorCode::Internal);
            }
            staging.renamed = true;
            at(Point::Renamed, &staging.file)?;
            self.directory.sync_all().map_err(io)?;
            // A failure here is an error/unknown outcome, never a rollback claim.
            self.store.unchanged()?;
            Ok(())
        }
    }
    #[cfg(test)]
    mod tests {
        include!("storage_tests.rs");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use native::ArtifactStore;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Clone)]
pub struct ArtifactStore;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl ArtifactStore {
    pub fn from_paths(_: &StoragePaths) -> BackendResult<Self> {
        Err(ErrorCode::Unavailable)
    }
    pub(super) fn acquire(&self) -> BackendResult<Self> {
        Err(ErrorCode::Unavailable)
    }
    pub(super) fn publish(&self, _: &[u8], _: &Cancellation) -> BackendResult<()> {
        Err(ErrorCode::Unavailable)
    }
}
