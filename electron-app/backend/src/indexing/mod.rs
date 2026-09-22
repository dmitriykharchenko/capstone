use crate::{
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};
use serde_json::Value;

mod coordinated;
mod gate;
mod journal;
mod worker;
pub use coordinated::{CoordinatedIndex, PreparedPublication, PublishedPaper};
pub use gate::{DerivedStoreGate, Readiness};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    Storage,
    InvalidJournal,
    InvalidPath,
    Conflict,
    Limit,
    RebuildRequired,
    NotReady,
    Poisoned,
    Cancelled,
}
pub type IndexResult<T> = Result<T, IndexError>;
impl From<crate::graph::GraphError> for IndexError {
    fn from(e: crate::graph::GraphError) -> Self {
        use crate::graph::GraphError::*;
        match e {
            Cancelled => Self::Cancelled,
            ArtifactTooLarge => Self::Limit,
            RebuildRequired => Self::RebuildRequired,
            _ => Self::InvalidJournal,
        }
    }
}
impl From<crate::retrieval::RetrievalError> for IndexError {
    fn from(e: crate::retrieval::RetrievalError) -> Self {
        use crate::retrieval::RetrievalError::*;
        match e {
            Cancelled => Self::Cancelled,
            ArtifactTooLarge => Self::Limit,
            ReindexRequired => Self::RebuildRequired,
            Conflict => Self::Conflict,
            _ => Self::InvalidJournal,
        }
    }
}
impl From<sqlite::Error> for IndexError {
    fn from(_: sqlite::Error) -> Self {
        Self::Storage
    }
}
impl From<std::io::Error> for IndexError {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}
impl From<crate::protocol::ErrorCode> for IndexError {
    fn from(e: crate::protocol::ErrorCode) -> Self {
        if e == crate::protocol::ErrorCode::Cancelled {
            Self::Cancelled
        } else {
            Self::Storage
        }
    }
}
impl From<IndexError> for crate::protocol::ErrorCode {
    fn from(e: IndexError) -> Self {
        if e == IndexError::Cancelled {
            Self::Cancelled
        } else {
            Self::Internal
        }
    }
}

pub trait Indexer: Send + Sync {
    fn index_papers(&self, context: RequestContext) -> BackendFuture<Value>;
    fn statuses(&self) -> BackendFuture<Value>;
    fn bm25_status(&self) -> BackendFuture<Value>;
    // Only new-app derived indexes; retain PDFs and imported conversation history.
    fn reset(&self, cancellation: Cancellation) -> BackendFuture<Value>;
}
