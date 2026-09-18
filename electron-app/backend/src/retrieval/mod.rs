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
