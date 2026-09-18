use crate::{
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};
use serde_json::Value;

pub trait Indexer: Send + Sync {
    fn index_papers(&self, context: RequestContext) -> BackendFuture<Value>;
    fn statuses(&self) -> BackendFuture<Value>;
    fn bm25_status(&self) -> BackendFuture<Value>;
    // Only new-app derived indexes; retain PDFs and imported conversation history.
    fn reset(&self, cancellation: Cancellation) -> BackendFuture<Value>;
}
