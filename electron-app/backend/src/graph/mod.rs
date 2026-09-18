use crate::{domain::GraphQuery, registry::BackendFuture, runtime::Cancellation};
use serde_json::Value;

pub trait GraphStore: Send + Sync {
    fn query(&self, query: GraphQuery, cancellation: Cancellation) -> BackendFuture<Value>;
    fn replace_paper(
        &self,
        paper_id: String,
        title: String,
        authors: Vec<String>,
        topics: Vec<String>,
        cancellation: Cancellation,
    ) -> BackendFuture<()>;
}

pub trait MetadataExtractor: Send + Sync {
    fn extract(&self, text: String, cancellation: Cancellation) -> BackendFuture<Value>;
}
