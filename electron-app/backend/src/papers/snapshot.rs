use super::{PaperError, PaperResult};
use crate::domain::{Chunk, PaperInfo};

/// Descriptor identity at capture time, not a lease or proof of future freshness.
/// The repository's pinned root is the namespace; rebuilding it may change that
/// namespace. Timestamps/inodes are change detectors, not adversarial guarantees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    pub device: u64,
    pub inode: u64,
    pub size_bytes: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
}
impl SourceIdentity {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn from_metadata(meta: &std::fs::Metadata) -> PaperResult<Self> {
        use std::os::unix::fs::MetadataExt;
        if !meta.is_file() {
            return Err(PaperError::UnsafePath);
        }
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            size_bytes: meta.len(),
            modified_seconds: meta.mtime(),
            modified_nanoseconds: meta.mtime_nsec(),
            changed_seconds: meta.ctime(),
            changed_nanoseconds: meta.ctime_nsec(),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(super) fn from_metadata(_: &std::fs::Metadata) -> PaperResult<Self> {
        Err(PaperError::UnsupportedPlatform)
    }
}

/// Metadata, raw SHA-256 and chunks all describe ONE captured byte sequence.
/// No OCR, provider calls, storage mutation or cross-store consistency is implied.
/// Parsing is synchronous native work: use OperationGroup::run_preparation to
/// own/join it. The descriptor is not held after capture; later changes are legal.
#[derive(Clone, Debug)]
pub struct PaperSourceSnapshot {
    pub info: PaperInfo,
    pub identity: SourceIdentity,
    pub source_sha256: String,
    pub chunks: Vec<Chunk>,
}
