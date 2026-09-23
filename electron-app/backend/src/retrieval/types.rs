use crate::{
    domain::{Chunk, RetrievalScope, RetrievalStrategy},
    protocol::ErrorCode,
    runtime::Cancellation,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrievalError {
    InvalidInput,
    InvalidPath,
    InvalidStore,
    ReindexRequired,
    Storage,
    Cancelled,
    History,
    CanonicalMissing,
    Provider,
    Conflict,
    UnsupportedArtifact,
    InvalidArtifact,
    ArtifactTooLarge,
    DigestMismatch,
}
pub type RetrievalResult<T> = Result<T, RetrievalError>;
impl std::fmt::Display for RetrievalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Retrieval operation failed ({self:?}).")
    }
}
impl std::error::Error for RetrievalError {}
impl From<sqlite::Error> for RetrievalError {
    fn from(_: sqlite::Error) -> Self {
        Self::Storage
    }
}
impl From<std::io::Error> for RetrievalError {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}
impl From<RetrievalError> for ErrorCode {
    fn from(e: RetrievalError) -> Self {
        match e {
            RetrievalError::Cancelled => Self::Cancelled,
            RetrievalError::InvalidInput | RetrievalError::InvalidPath => Self::InvalidParams,
            _ => Self::Internal,
        }
    }
}
pub(super) fn check(c: &Cancellation) -> RetrievalResult<()> {
    c.check().map_err(|_| RetrievalError::Cancelled)
}
pub(super) fn seal(c: &Cancellation) -> RetrievalResult<crate::runtime::CommitPermit> {
    c.begin_commit().map_err(|e| {
        if e == ErrorCode::Cancelled {
            RetrievalError::Cancelled
        } else {
            RetrievalError::Conflict
        }
    })
}
pub(super) fn nonempty(s: &str) -> RetrievalResult<()> {
    if s.chars().all(python_whitespace) {
        Err(RetrievalError::InvalidInput)
    } else {
        Ok(())
    }
}
pub(super) fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Include provider/model (and any embedding-mode identity) in `model`.
/// A different identity, even at the same dimension, requires explicit rebuild.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingIdentity {
    pub model: String,
    pub dimension: usize,
}
impl EmbeddingIdentity {
    pub fn validate(&self) -> RetrievalResult<()> {
        nonempty(&self.model)?;
        if self.dimension == 0 || self.dimension > 65_536 {
            return Err(RetrievalError::InvalidInput);
        }
        Ok(())
    }
    pub(super) fn vector(&self, vector: &[f64]) -> RetrievalResult<()> {
        if vector.len() != self.dimension {
            return Err(RetrievalError::ReindexRequired);
        }
        if vector.iter().any(|v| !v.is_finite()) {
            return Err(RetrievalError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedChunk {
    pub id: String,
    pub paper_id: String,
    pub chunk_index: u32,
    pub filename: String,
    pub title: String,
    pub page: u32,
    pub text: String,
    pub vector: Vec<f64>,
}
impl EmbeddedChunk {
    pub fn from_chunk(id: String, chunk: Chunk, vector: Vec<f64>) -> Self {
        Self {
            id,
            paper_id: chunk.source.paper_id,
            chunk_index: chunk.source.chunk_index,
            filename: chunk.source.filename,
            title: chunk.source.title,
            page: chunk.source.page,
            text: chunk.text,
            vector,
        }
    }
    pub(super) fn validate(&self, identity: &EmbeddingIdentity) -> RetrievalResult<()> {
        for s in [
            &self.id,
            &self.paper_id,
            &self.filename,
            &self.title,
            &self.text,
        ] {
            nonempty(s)?;
        }
        if self.page == 0 {
            return Err(RetrievalError::InvalidInput);
        }
        identity.vector(&self.vector)
    }
}

/// Immutable validated input; construction and embedding do not mutate storage.
#[derive(Clone, Debug)]
pub struct PreparedPaper {
    pub(super) paper_id: String,
    pub(super) identity: EmbeddingIdentity,
    pub(super) chunks: Vec<EmbeddedChunk>,
}
impl PreparedPaper {
    pub fn new(
        paper_id: String,
        identity: EmbeddingIdentity,
        chunks: Vec<EmbeddedChunk>,
    ) -> RetrievalResult<Self> {
        nonempty(&paper_id)?;
        identity.validate()?;
        let mut ids = BTreeSet::new();
        let mut indices = BTreeSet::new();
        for chunk in &chunks {
            chunk.validate(&identity)?;
            if chunk.paper_id != paper_id
                || !ids.insert(&chunk.id)
                || !indices.insert(chunk.chunk_index)
            {
                return Err(RetrievalError::InvalidInput);
            }
        }
        Ok(Self {
            paper_id,
            identity,
            chunks,
        })
    }
    pub fn paper_id(&self) -> &str {
        &self.paper_id
    }
    pub fn chunks(&self) -> &[EmbeddedChunk] {
        &self.chunks
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ExchangeVector {
    pub exchange_id: i64,
    pub session_id: String,
    pub context: String,
    pub vector: Vec<f64>,
}
impl ExchangeVector {
    pub(super) fn validate(&self, identity: &EmbeddingIdentity) -> RetrievalResult<()> {
        if self.exchange_id <= 0 {
            return Err(RetrievalError::InvalidInput);
        }
        nonempty(&self.session_id)?;
        nonempty(&self.context)?;
        identity.vector(&self.vector)
    }
}
/// Constructed only by canonical-history preparation on `NativeRetriever`.
#[derive(Clone, Debug)]
pub struct PreparedExchange {
    pub(super) identity: EmbeddingIdentity,
    pub(super) record: ExchangeVector,
}

#[derive(Clone, Debug)]
pub struct PreparedReset {
    pub(super) identity: EmbeddingIdentity,
}
impl PreparedReset {
    /// Explicitly discard BOTH derived collections and adopt this identity.
    /// This never resets or writes canonical history/PDFs.
    pub fn new(identity: EmbeddingIdentity) -> RetrievalResult<Self> {
        identity.validate()?;
        Ok(Self { identity })
    }
}
#[derive(Clone, Debug)]
pub enum PreparedOperation {
    ReplacePaper(PreparedPaper),
    DeletePaper(String),
    UpsertExchange(PreparedExchange),
    DeleteExchange(i64),
    Reset(PreparedReset),
    RebuildLexical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    pub strategy: RetrievalStrategy,
    pub scope: RetrievalScope,
    pub limit: usize,
}
impl SearchRequest {
    pub fn validate(&self) -> RetrievalResult<()> {
        nonempty(&self.query)?;
        if self.limit == 0 {
            return Err(RetrievalError::InvalidInput);
        }
        // The legacy chat path is hybrid only; explicit strategies use FullCorpus.
        if self.scope == RetrievalScope::ChatCandidates
            && self.strategy != RetrievalStrategy::Hybrid
        {
            return Err(RetrievalError::InvalidInput);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PaperMatch {
    pub paper_id: String,
    pub title: String,
    pub filename: String,
    pub page: u32,
    pub snippet: String,
    pub score: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LexicalStatus {
    Ready,
    Stale,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexStatus {
    pub identity: EmbeddingIdentity,
    pub revision: i64,
    pub chunk_count: usize,
    pub paper_count: usize,
    pub exchange_count: usize,
    pub lexical: LexicalStatus,
}
#[derive(Clone, Debug)]
pub struct IndexSnapshot {
    pub status: IndexStatus,
    pub chunks: Vec<EmbeddedChunk>,
    pub(super) exchanges: Vec<ExchangeVector>,
    pub(super) lexical: super::ranking::Bm25Index,
}
