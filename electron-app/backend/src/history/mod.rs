use std::path::PathBuf;

use crate::{
    domain::{Message, SourceRecord},
    registry::BackendFuture,
    runtime::Cancellation,
};
use serde_json::Value;

pub trait HistoryStore: Send + Sync {
    fn sessions(&self) -> BackendFuture<Value>;
    fn messages(&self, session_id: String) -> BackendFuture<Vec<Message>>;
    fn append_exchange(
        &self,
        session_id: String,
        user: String,
        assistant: String,
        sources: Vec<SourceRecord>,
        cancellation: Cancellation,
    ) -> BackendFuture<i64>;
    // Must use a consistent SQLite snapshot, including committed WAL contents;
    // never filesystem-copy a live .db alone or mutate the source. Destination
    // is exclusively new-app storage and must not overwrite an existing history.
    fn import_snapshot(
        &self,
        read_only_source: PathBuf,
        cancellation: Cancellation,
    ) -> BackendFuture<Value>;
}
