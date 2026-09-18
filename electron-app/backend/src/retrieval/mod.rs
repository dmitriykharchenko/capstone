//! Exact native squared-L2 retrieval with app-owned, versioned SQLite storage.
//!
//! Prepare operations without publication, then apply a whole batch once. `apply`
//! seals its token at its ONE final COMMIT. Never reuse that token for another
//! paper/store, or replace it with an unlinked token to evade cancellation.
//! Cross-store publication/reconciliation belongs to the indexing coordinator.
//! Synchronous store methods must run on a joined blocking worker; `NativeRetriever`
//! provides that boundary for the async trait and service operations.
mod publication;
mod ranking;
mod service;
mod store;
mod types;

pub use ranking::{bm25_scores, hybrid_scores, similarity, squared_l2, tokenize};
pub use service::{HistorySearchRequest, NativeRetriever};
pub use store::VectorStore;
pub use types::{
    EmbeddedChunk, EmbeddingIdentity, IndexSnapshot, IndexStatus, LexicalStatus, PaperMatch,
    PreparedExchange, PreparedOperation, PreparedPaper, PreparedReset, RetrievalError,
    RetrievalResult, SearchRequest,
};

use crate::{
    domain::{Chunk, RetrievalScope, RetrievalStrategy},
    registry::BackendFuture,
    runtime::Cancellation,
};
use serde_json::Value;

pub trait Retriever: Send + Sync {
    fn retrieve(
        &self,
        query: String,
        strategy: RetrievalStrategy,
        scope: RetrievalScope,
        limit: usize,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Chunk>>;
    fn search_history(
        &self,
        query: String,
        session_id: Option<String>,
        limit: usize,
        cancellation: Cancellation,
    ) -> BackendFuture<Value>;
    fn index_exchange(&self, exchange_id: i64, cancellation: Cancellation) -> BackendFuture<()>;
}

#[cfg(test)]
mod rag_native_cancellation {
    include!("../../tests/rag_native_cancellation.rs");
    rag_native_cancellation_tests!();
}
