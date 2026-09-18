use crate::{registry::BackendFuture, runtime::RequestContext};
use serde_json::Value;

pub trait ChatService: Send + Sync {
    fn turn(
        &self,
        session_id: String,
        message: String,
        context: RequestContext,
    ) -> BackendFuture<Value>;
}
