use super::{NativeChat, service::history_error};
use crate::{
    history::{SqliteHistory, TurnRequest},
    protocol::ErrorCode,
    registry::{BackendFuture, CommandHandler, Registry},
    retrieval::HistorySearchRequest,
    runtime::{CancellationMode, RequestContext},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default = "limit")]
    limit: usize,
}
fn limit() -> usize {
    10
}

/// Canonical reads remain installed when derived readiness/provider config fails.
pub fn install_commands(registry: &mut Registry, history: SqliteHistory, chat: Option<NativeChat>) {
    for (command, owner) in [
        ("list_sessions", "history"),
        ("get_messages", "history"),
        ("chat", "chat"),
        ("chat_stream", "chat"),
        ("search", "retrieval"),
    ] {
        if chat.is_none() && !matches!(command, "list_sessions" | "get_messages") {
            continue;
        }
        registry
            .install(
                command,
                owner,
                Arc::new(Handler {
                    command,
                    history: history.clone(),
                    chat: chat.clone(),
                }),
            )
            .expect("native Chat ownership slots");
    }
}
struct Handler {
    command: &'static str,
    history: SqliteHistory,
    chat: Option<NativeChat>,
}
impl CommandHandler for Handler {
    fn cancellation_mode(&self) -> CancellationMode {
        CancellationMode::Cooperative
    }
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let command = self.command;
        let history = self.history.clone();
        let chat = self.chat.clone();
        Box::pin(async move {
            let group = context.operation_group()?;
            let result = match command {
                "chat" | "chat_stream" => {
                    return chat
                        .ok_or(ErrorCode::Unavailable)?
                        .turn(
                            serde_json::from_value::<TurnRequest>(params)
                                .map_err(|_| ErrorCode::InvalidParams)?,
                            context,
                        )
                        .await;
                }
                "list_sessions" => {
                    serde_json::from_value::<Empty>(params)
                        .map_err(|_| ErrorCode::InvalidParams)?;
                    group
                        .run_preparation("sessions", move |_| async move {
                            Ok(json!({"sessions":history.list_sessions().map_err(history_error)?}))
                        })
                        .await?
                }
                "get_messages" => {
                    let session = serde_json::from_value::<Session>(params)
                        .map_err(|_| ErrorCode::InvalidParams)?
                        .session_id;
                    if session.is_empty() || session.len() > 128 {
                        return Err(ErrorCode::InvalidParams);
                    }
                    let id = session.clone();
                    let h = history.clone();
                    let messages = group
                        .run_preparation("messages", move |_| async move {
                            h.get_messages(&id).map_err(history_error)
                        })
                        .await?;
                    let mut messages =
                        serde_json::to_value(messages).map_err(|_| ErrorCode::Internal)?;
                    for message in messages.as_array_mut().ok_or(ErrorCode::Internal)? {
                        if let Some(id) = message["exchange_id"].as_i64() {
                            let h = history.clone();
                            let intent = group
                                .run_preparation("message-turn", move |_| async move {
                                    Ok(h.verified_intent(id))
                                })
                                .await?;
                            let (status, observed, _) = if let Some(chat) = &chat {
                                chat.index_status(id).await
                            } else {
                                ("blocked", Value::Null, false)
                            };
                            message["turn_id"] =
                                intent.ok().map_or(Value::Null, |c| json!(c.intent.turn_id));
                            message["history_index_status"] = json!(status);
                            message["history_index_status_observed_at"] = observed;
                        }
                    }
                    json!({"session_id":session,"messages":messages})
                }
                "search" => {
                    let request = serde_json::from_value::<Search>(params)
                        .map_err(|_| ErrorCode::InvalidParams)?;
                    if request.query.trim().is_empty()
                        || request.query.len() > 64 * 1024
                        || !(1..=100).contains(&request.limit)
                        || request
                            .session_id
                            .as_ref()
                            .is_some_and(|s| s.is_empty() || s.len() > 128)
                    {
                        return Err(ErrorCode::InvalidParams);
                    }
                    group
                        .run_preparation("history-search", move |c| async move {
                            chat.ok_or(ErrorCode::Unavailable)?
                                .search(
                                    HistorySearchRequest {
                                        query: request.query,
                                        session_id: request.session_id,
                                        limit: request.limit,
                                    },
                                    c,
                                )
                                .await
                        })
                        .await?
                }
                _ => return Err(ErrorCode::Unavailable),
            };
            let _seal = group.seal_completion()?;
            Ok(result)
        })
    }
}
