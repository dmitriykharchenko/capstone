pub mod chunking;
mod extraction;
mod repository;
mod safe_root;
mod snapshot;

pub use extraction::{ExtractedPage, MAX_EXTRACTED_BYTES, MAX_PDF_PAGES, extract_pdf_pages};
pub use repository::{
    NativePaperRepository, PaperCommand, PaperCommandHandler, PaperRequest, PdfChunk,
};
pub use snapshot::{PaperSourceSnapshot, SourceIdentity};

use crate::{
    domain::{Chunk, PaperInfo, PdfInfo},
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};

pub const MAX_PDF_BYTES: u64 = 64 * 1024 * 1024;
pub const PDF_CHUNK_BYTES: usize = 64 * 1024;

/// Local, path-free errors retain failure reasons for indexing and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaperError {
    InvalidId,
    NotFound,
    UnsafePath,
    Io,
    EmptyFile,
    TooLarge,
    ChangedDuringRead,
    CorruptPdf,
    ExtractionLimit,
    Cancelled,
    UnsupportedPlatform,
}

pub type PaperResult<T> = Result<T, PaperError>;

impl From<PaperError> for crate::protocol::ErrorCode {
    fn from(error: PaperError) -> Self {
        use crate::protocol::ErrorCode;
        match error {
            PaperError::Cancelled => ErrorCode::Cancelled,
            PaperError::Io | PaperError::ChangedDuringRead => ErrorCode::Internal,
            PaperError::UnsupportedPlatform => ErrorCode::Unavailable,
            _ => ErrorCode::InvalidParams,
        }
    }
}

pub fn is_safe_id(id: &str) -> bool {
    id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// Match the legacy native filename-title heuristic exactly.
pub fn derive_title(id: &str) -> String {
    let lowered = id.to_ascii_lowercase();
    let mut stripped = id;
    'prefix: for kind in ["conf", "journals"] {
        for a in ["_", "-", ""] {
            for b in ["_", "-"] {
                let prefix = format!("dblp{a}{kind}{b}");
                if lowered.starts_with(&prefix) {
                    stripped = &id[prefix.len()..];
                    break 'prefix;
                }
            }
        }
    }
    let cleaned = stripped.replace(['_', '-'], " ");
    if cleaned.trim().is_empty() {
        id.to_owned()
    } else {
        cleaned.trim().to_owned()
    }
}

pub trait PaperRepository: Send + Sync {
    fn list(&self) -> BackendFuture<Vec<PaperInfo>>;
    fn get(&self, paper_id: String) -> BackendFuture<PaperInfo>;
    // Emit pdf_chunk with offset + bounded bytes, then return metadata/byte count.
    // Never place an entire arbitrary-sized PDF into a single JSON frame.
    fn read_pdf(&self, paper_id: String, context: RequestContext) -> BackendFuture<PdfInfo>;
    fn extract_chunks(
        &self,
        paper_id: String,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Chunk>>;
}
