use crate::{
    domain::Message,
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};

pub trait ModelClient: Send + Sync {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation)
    -> BackendFuture<String>;
    fn stream(&self, messages: Vec<Message>, context: RequestContext) -> BackendFuture<String>;
}

pub trait EmbeddingClient: Send + Sync {
    fn embed(&self, texts: Vec<String>, cancellation: Cancellation)
    -> BackendFuture<Vec<Vec<f64>>>;
}
