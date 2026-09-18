use crate::{
    domain::{Chunk, PaperInfo, PdfInfo},
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};

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
