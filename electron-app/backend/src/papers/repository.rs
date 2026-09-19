use std::{io::Read, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    chunking::{DEFAULT_MAX_CHUNK_BYTES, split_into_paragraphs, split_oversized_chunk},
    extraction::check,
    safe_root::SafeRoot,
    *,
};
use crate::{
    domain::{Chunk, PaperInfo, PdfInfo, SourceRecord},
    protocol::ErrorCode,
    registry::{BackendFuture, CommandHandler},
    runtime::{Cancellation, RequestContext},
};

#[derive(Clone)]
pub struct NativePaperRepository {
    root: Arc<SafeRoot>,
}

impl NativePaperRepository {
    /// Pin an approved read-only root. A missing directory lists as empty;
    /// reconstruct the repository if that directory is created later.
    pub fn new(directory: impl AsRef<Path>) -> PaperResult<Self> {
        Ok(Self {
            root: Arc::new(SafeRoot::new(directory.as_ref())?),
        })
    }

    pub fn list_papers(&self) -> PaperResult<Vec<PaperInfo>> {
        let mut papers = Vec::new();
        for id in self.root.ids()? {
            match self.get_paper(&id) {
                Ok(paper) => papers.push(paper),
                Err(PaperError::NotFound | PaperError::UnsafePath) => continue,
                Err(error) => return Err(error),
            }
        }
        papers.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(papers)
    }

    pub fn get_paper(&self, paper_id: &str) -> PaperResult<PaperInfo> {
        let file = self.root.open(paper_id)?;
        let size_bytes = file.metadata().map_err(|_| PaperError::Io)?.len();
        Ok(info(paper_id, size_bytes))
    }

    /// Snapshot through the same checked descriptor, never re-open by path.
    /// Reject growth/short reads/mutation and oversize rather than truncate.
    pub fn read_pdf_bytes(
        &self,
        paper_id: &str,
        cancellation: &Cancellation,
    ) -> PaperResult<Vec<u8>> {
        Ok(self.capture(paper_id, cancellation, || {})?.0)
    }

    fn capture(
        &self,
        paper_id: &str,
        cancellation: &Cancellation,
        after_read: impl FnOnce(),
    ) -> PaperResult<(Vec<u8>, SourceIdentity)> {
        check(cancellation)?;
        let mut file = self.root.open(paper_id)?;
        let before = file.metadata().map_err(|_| PaperError::Io)?;
        if before.len() == 0 {
            return Err(PaperError::EmptyFile);
        }
        if before.len() > MAX_PDF_BYTES {
            return Err(PaperError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        let mut buffer = [0; PDF_CHUNK_BYTES];
        loop {
            check(cancellation)?;
            let count = file.read(&mut buffer).map_err(|_| PaperError::Io)?;
            if count == 0 {
                break;
            }
            if bytes.len() as u64 + count as u64 > MAX_PDF_BYTES {
                return Err(PaperError::TooLarge);
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        after_read();
        let after = file.metadata().map_err(|_| PaperError::Io)?;
        if bytes.len() as u64 != before.len() || !self.root.unchanged(&before, &after) {
            return Err(PaperError::ChangedDuringRead);
        }
        check(cancellation)?;
        Ok((bytes, SourceIdentity::from_metadata(&before)?))
    }

    /// Hash and extract the same bounded capture; never reopen between those
    /// steps. Metadata also comes from that descriptor, not get_paper().
    pub fn prepare_for_index(
        &self,
        paper_id: &str,
        cancellation: &Cancellation,
    ) -> PaperResult<PaperSourceSnapshot> {
        self.prepare_inner(paper_id, cancellation, || {})
    }

    fn prepare_inner(
        &self,
        paper_id: &str,
        cancellation: &Cancellation,
        after_capture: impl FnOnce(),
    ) -> PaperResult<PaperSourceSnapshot> {
        let (bytes, identity) = self.capture(paper_id, cancellation, || {})?;
        after_capture();
        check(cancellation)?;
        let source_sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let pages = extract_pdf_pages(&bytes, cancellation)?;
        let chunks = chunks_from_pages(paper_id, pages, cancellation)?;
        check(cancellation)?;
        Ok(PaperSourceSnapshot {
            info: info(paper_id, identity.size_bytes),
            identity,
            source_sha256,
            chunks,
        })
    }

    /// Cheap later revalidation against this pinned root. A match is only an
    /// observation NOW, not a lock: for content proof capture/hash again. Even a
    /// new hash cannot prevent another writer changing the source afterwards.
    pub fn source_identity(
        &self,
        paper_id: &str,
        cancellation: &Cancellation,
    ) -> PaperResult<SourceIdentity> {
        check(cancellation)?;
        let meta = self
            .root
            .open(paper_id)?
            .metadata()
            .map_err(|_| PaperError::Io)?;
        check(cancellation)?;
        SourceIdentity::from_metadata(&meta)
    }

    pub fn extract_pages(
        &self,
        paper_id: &str,
        cancellation: &Cancellation,
    ) -> PaperResult<Vec<ExtractedPage>> {
        extract_pdf_pages(&self.read_pdf_bytes(paper_id, cancellation)?, cancellation)
    }

    pub fn chunks(&self, paper_id: &str, cancellation: &Cancellation) -> PaperResult<Vec<Chunk>> {
        let pages = self.extract_pages(paper_id, cancellation)?;
        chunks_from_pages(paper_id, pages, cancellation)
    }
}

fn chunks_from_pages(
    paper_id: &str,
    pages: Vec<ExtractedPage>,
    cancellation: &Cancellation,
) -> PaperResult<Vec<Chunk>> {
    let mut chunks = Vec::new();
    for page in pages {
        check(cancellation)?;
        for paragraph in split_into_paragraphs(&page.text) {
            for text in split_oversized_chunk(&paragraph, DEFAULT_MAX_CHUNK_BYTES)
                .map_err(|_| PaperError::ExtractionLimit)?
            {
                check(cancellation)?;
                let chunk_index =
                    u32::try_from(chunks.len()).map_err(|_| PaperError::ExtractionLimit)?;
                chunks.push(Chunk {
                    source: SourceRecord {
                        // Ranking/scoring belong to retrieval, not ingestion.
                        rank: 0,
                        score: 0.0,
                        paper_id: paper_id.to_owned(),
                        chunk_index,
                        filename: format!("{paper_id}.pdf"),
                        title: derive_title(paper_id),
                        page: page.page_number,
                        snippet: text.chars().take(240).collect(),
                    },
                    text,
                });
            }
        }
    }
    Ok(chunks)
}

fn info(id: &str, size_bytes: u64) -> PaperInfo {
    PaperInfo {
        id: id.to_owned(),
        filename: format!("{id}.pdf"),
        title: derive_title(id),
        size_bytes,
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod snapshot_tests;

fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> PaperResult<T> + Send + 'static,
) -> BackendFuture<T> {
    Box::pin(async move {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| ErrorCode::Internal)?
            .map_err(Into::into)
    })
}

impl PaperRepository for NativePaperRepository {
    fn list(&self) -> BackendFuture<Vec<PaperInfo>> {
        let repository = self.clone();
        blocking(move || repository.list_papers())
    }

    fn get(&self, paper_id: String) -> BackendFuture<PaperInfo> {
        let repository = self.clone();
        blocking(move || repository.get_paper(&paper_id))
    }

    fn read_pdf(&self, paper_id: String, context: RequestContext) -> BackendFuture<PdfInfo> {
        let repository = self.clone();
        Box::pin(async move {
            let cancellation = context.cancellation();
            let id = paper_id.clone();
            let bytes = blocking(move || repository.read_pdf_bytes(&id, &cancellation)).await?;
            for (index, chunk) in bytes.chunks(PDF_CHUNK_BYTES).enumerate() {
                let event = PdfChunk {
                    offset: (index * PDF_CHUNK_BYTES) as u64,
                    bytes: chunk.to_vec(),
                };
                context
                    .emit(
                        "pdf_chunk",
                        serde_json::to_value(event).map_err(|_| ErrorCode::Internal)?,
                    )
                    .await?;
            }
            context.cancellation().check()?;
            Ok(PdfInfo {
                id: paper_id.clone(),
                filename: format!("{paper_id}.pdf"),
                mime_type: "application/pdf".to_owned(),
                total_bytes: bytes.len() as u64,
            })
        })
    }

    fn extract_chunks(
        &self,
        paper_id: String,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Chunk>> {
        let repository = self.clone();
        blocking(move || repository.chunks(&paper_id, &cancellation))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaperRequest {
    pub paper_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PdfChunk {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {}

#[derive(Clone, Copy)]
pub enum PaperCommand {
    List,
    Get,
    ReadPdf,
}

/// Validated adapters, intentionally not installed in the production registry.
pub struct PaperCommandHandler {
    repository: NativePaperRepository,
    command: PaperCommand,
}

impl PaperCommandHandler {
    pub fn new(repository: NativePaperRepository, command: PaperCommand) -> Self {
        Self {
            repository,
            command,
        }
    }
}

impl CommandHandler for PaperCommandHandler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let repository = self.repository.clone();
        let command = self.command;
        Box::pin(async move {
            if matches!(command, PaperCommand::List) {
                serde_json::from_value::<ListRequest>(params)
                    .map_err(|_| ErrorCode::InvalidParams)?;
                return serde_json::to_value(repository.list().await?)
                    .map_err(|_| ErrorCode::Internal);
            }
            let request: PaperRequest =
                serde_json::from_value(params).map_err(|_| ErrorCode::InvalidParams)?;
            if !is_safe_id(&request.paper_id) {
                return Err(ErrorCode::InvalidParams);
            }
            match command {
                PaperCommand::Get => serde_json::to_value(repository.get(request.paper_id).await?),
                PaperCommand::ReadPdf => {
                    serde_json::to_value(repository.read_pdf(request.paper_id, context).await?)
                }
                PaperCommand::List => unreachable!(),
            }
            .map_err(|_| ErrorCode::Internal)
        })
    }
}
