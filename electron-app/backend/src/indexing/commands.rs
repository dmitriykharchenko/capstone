use super::NativeIndexer;
use crate::{
    protocol::ErrorCode,
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{CancellationMode, RequestContext},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexRequest {
    #[serde(default)]
    reindex: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// Only trusted startup installs these after recovery. A blocked installation
/// exposes status diagnostics, never a reset bypass or provider-backed commands.
pub fn install_commands(registry: &mut Registry, indexer: Result<NativeIndexer, &'static str>) {
    let (service, blocked) = match indexer {
        Ok(s) => (Some(s), "ready"),
        Err(reason) => (None, reason),
    };
    for command in [
        "index_papers",
        "index_papers_stream",
        "paper_statuses",
        "bm25_status",
        "reset_all_indices",
    ] {
        if service.is_none() && !matches!(command, "paper_statuses" | "bm25_status") {
            continue;
        }
        registry
            .install(
                command,
                "indexing",
                Arc::new(Handler {
                    service: service.clone(),
                    blocked,
                    command,
                }),
            )
            .expect("checked-in indexing ownership slots");
    }
}
struct Handler {
    service: Option<NativeIndexer>,
    blocked: &'static str,
    command: &'static str,
}
impl CommandHandler for Handler {
    fn cancellation_mode(&self) -> CancellationMode {
        CancellationMode::Cooperative
    }
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let service = self.service.clone();
        let command = self.command;
        let blocked = self.blocked;
        Box::pin(async move {
            let indexing = matches!(command, "index_papers" | "index_papers_stream");
            let reindex = if indexing {
                serde_json::from_value::<IndexRequest>(params)
                    .map_err(|_| ErrorCode::InvalidParams)?
                    .reindex
            } else {
                serde_json::from_value::<Empty>(params).map_err(|_| ErrorCode::InvalidParams)?;
                false
            };
            let result = if let Some(service) = service {
                if indexing {
                    return service
                        .index_papers(context, reindex, command == "index_papers_stream")
                        .await;
                }
                match command {
                    "reset_all_indices" => return service.reset(&context).await,
                    "paper_statuses" => service.statuses(&context).await?,
                    "bm25_status" => service.bm25_status(&context).await?,
                    _ => return Err(ErrorCode::Unavailable),
                }
            } else if command == "paper_statuses" {
                json!({"readiness":blocked,"statuses":[]})
            } else {
                json!({"readiness":blocked,"built":false,"chunk_count":0,"paper_ids":[]})
            };
            let _seal = context.operation_group()?.seal_completion()?;
            Ok(result)
        })
    }
}
