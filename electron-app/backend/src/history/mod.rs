use std::path::PathBuf;

use crate::{
    domain::{Message, SourceRecord},
    registry::BackendFuture,
    runtime::Cancellation,
};
use serde_json::Value;

mod migration;
mod schema;
mod store;
mod turns;
pub use migration::ImportReport;
pub use store::{
    ExchangeRecord, HistoryError, HistoryResult, PaperStatusRecord, SessionRecord, SqliteHistory,
};
pub use turns::{Admission, CommittedTurn, ExchangeIntent, TurnRequest};

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

impl HistoryStore for SqliteHistory {
    fn sessions(&self) -> BackendFuture<Value> {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                store
                    .list_sessions()
                    .map(|sessions| serde_json::json!({"sessions":sessions}))
            })
            .await
            .map_err(|_| crate::protocol::ErrorCode::Internal)?
            .map_err(Into::into)
        })
    }
    fn messages(&self, session_id: String) -> BackendFuture<Vec<Message>> {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.get_messages(&session_id))
                .await
                .map_err(|_| crate::protocol::ErrorCode::Internal)?
                .map_err(Into::into)
        })
    }
    fn append_exchange(
        &self,
        session_id: String,
        user: String,
        assistant: String,
        sources: Vec<SourceRecord>,
        cancellation: Cancellation,
    ) -> BackendFuture<i64> {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                store.append_exchange_sync(&session_id, &user, &assistant, &sources, &cancellation)
            })
            .await
            .map_err(|_| crate::protocol::ErrorCode::Internal)?
            .map_err(Into::into)
        })
    }
    fn import_snapshot(
        &self,
        read_only_source: PathBuf,
        cancellation: Cancellation,
    ) -> BackendFuture<Value> {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                store
                    .import_legacy(&read_only_source, &cancellation)
                    .map(|report| serde_json::json!(report))
            })
            .await
            .map_err(|_| crate::protocol::ErrorCode::Internal)?
            .map_err(Into::into)
        })
    }
}
