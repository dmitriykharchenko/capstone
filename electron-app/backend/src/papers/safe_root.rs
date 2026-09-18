//! Descriptor-relative access: no check-by-path followed by read-by-path race.
use super::{PaperError, PaperResult, is_safe_id};
use std::{
    fs::{File, Metadata},
    path::Path,
};

pub(super) struct SafeRoot {
    directory: Option<File>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        io,
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
        path::Component,
    };

    fn io_error(error: io::Error) -> PaperError {
        match error.raw_os_error() {
            Some(libc::ENOENT) => PaperError::NotFound,
            Some(libc::ELOOP | libc::ENOTDIR) => PaperError::UnsafePath,
            _ => PaperError::Io,
        }
    }

    fn open_at(parent: &File, name: &CStr, directory: bool) -> PaperResult<File> {
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent is live, name is NUL-terminated, and no O_CREAT means no mode argument.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        // SAFETY: openat returned a fresh owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: fdopendir transferred ownership to this local guard exactly once.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    unsafe fn errno() -> *mut libc::c_int {
        #[cfg(target_os = "macos")]
        {
            unsafe { libc::__error() }
        }
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::__errno_location() }
        }
    }

    impl SafeRoot {
        pub(in crate::papers) fn new(path: &Path) -> PaperResult<Self> {
            let canonical = match path.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Self { directory: None });
                }
                Err(error) => return Err(io_error(error)),
            };
            // Follow the configured root once, then walk its canonical components
            // without following any symlink introduced during the walk.
            let mut directory = File::open("/").map_err(io_error)?;
            for component in canonical.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        let name =
                            CString::new(name.as_bytes()).map_err(|_| PaperError::UnsafePath)?;
                        directory = open_at(&directory, &name, true)?;
                    }
                    _ => return Err(PaperError::UnsafePath),
                }
            }
            Ok(Self {
                directory: Some(directory),
            })
        }

        pub(in crate::papers) fn open(&self, id: &str) -> PaperResult<File> {
            if !is_safe_id(id) {
                return Err(PaperError::InvalidId);
            }
            let directory = self.directory.as_ref().ok_or(PaperError::NotFound)?;
            let name = CString::new(format!("{id}.pdf")).map_err(|_| PaperError::InvalidId)?;
            let file = open_at(directory, &name, false)?;
            if !file.metadata().map_err(io_error)?.is_file() {
                return Err(PaperError::UnsafePath);
            }
            Ok(file)
        }

        pub(in crate::papers) fn ids(&self) -> PaperResult<Vec<String>> {
            let Some(directory) = &self.directory else {
                return Ok(Vec::new());
            };
            // A separately opened directory stream has its own cursor, allowing
            // repeated/concurrent listings of the pinned root after path replacement.
            let fd = open_at(directory, c".", true)?.into_raw_fd();
            // SAFETY: fd is a fresh owned directory descriptor.
            let pointer = unsafe { libc::fdopendir(fd) };
            if pointer.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: failed fdopendir did not consume fd.
                unsafe {
                    libc::close(fd);
                }
                return Err(io_error(error));
            }
            let stream = Directory(pointer);
            let mut ids = Vec::new();
            loop {
                // SAFETY: stream is live and only used here; errno is thread-local.
                let entry = unsafe {
                    *errno() = 0;
                    libc::readdir(stream.0)
                };
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(0) {
                        return Err(io_error(error));
                    }
                    break;
                }
                // SAFETY: readdir's entry remains live until the next call, and d_name is terminated.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if let Ok(name) = name.to_str()
                    && let Some(id) = name.strip_suffix(".pdf").filter(|id| is_safe_id(id))
                {
                    ids.push(id.to_owned());
                }
            }
            Ok(ids)
        }

        pub(in crate::papers) fn unchanged(&self, before: &Metadata, after: &Metadata) -> bool {
            before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec()
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl SafeRoot {
    pub(super) fn new(_: &Path) -> PaperResult<Self> {
        Err(PaperError::UnsupportedPlatform)
    }
    pub(super) fn open(&self, _: &str) -> PaperResult<File> {
        Err(PaperError::UnsupportedPlatform)
    }
    pub(super) fn ids(&self) -> PaperResult<Vec<String>> {
        Err(PaperError::UnsupportedPlatform)
    }
    pub(super) fn unchanged(&self, _: &Metadata, _: &Metadata) -> bool {
        false
    }
}
