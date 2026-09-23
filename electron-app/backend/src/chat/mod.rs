use crate::{registry::BackendFuture, runtime::RequestContext};
use serde_json::Value;
mod commands;
mod diagnostics;
mod service;
pub use commands::install_commands;
pub use service::NativeChat;

pub trait ChatService: Send + Sync {
    fn turn(
        &self,
        session_id: String,
        message: String,
        context: RequestContext,
    ) -> BackendFuture<Value>;
}
