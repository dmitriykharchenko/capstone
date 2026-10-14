//! Chat turn streaming, session history and history search over the Python bridge.

use serde_json::Value;
use tauri::ipc::Channel;

use crate::error::CommandError;
use crate::{bridge, project_root, run_blocking};

const MAX_CHAT_SOURCES: usize = 35;
const MAX_CHAT_INITIAL_SUBQUERIES: usize = 5;
const MAX_CHAT_SUBQUERIES: usize = 7;
const CHAT_TRANSPORT_ERROR: &str = "Chat transport failed. Check backend logs and retry.";

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct ChatSubquery {
    id: String,
    index: u32,
    query: String,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct ChatSource {
    rank: u32,
    paper_id: String,
    chunk_index: u32,
    filename: String,
    title: String,
    page: u32,
    score: f64,
    snippet: String,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ChatCoverageStatus {
    Reviewing,
    Complete,
    Followup,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ChatStreamEvent {
    Plan {
        subqueries: Vec<ChatSubquery>,
    },
    Subquery {
        id: String,
        index: u32,
        total: u32,
        query: String,
    },
    Subanswer {
        id: String,
        index: u32,
        total: u32,
        query: String,
        answer: String,
        sources: Vec<ChatSource>,
    },
    Coverage {
        status: ChatCoverageStatus,
        total: u32,
        subqueries: Vec<ChatSubquery>,
    },
    Delta {
        delta: String,
    },
    Done {
        reply: String,
        chat_mode: String,
        indexed: bool,
        sources: Vec<ChatSource>,
    },
    Error {
        message: String,
    },
}

fn chat_sources(
    value: &Value,
    max_sources: usize,
    allow_empty: bool,
) -> Result<Vec<ChatSource>, ()> {
    let sources: Vec<ChatSource> =
        serde_json::from_value(value.get("sources").cloned().ok_or(())?).map_err(|_| ())?;
    if (!allow_empty && sources.is_empty()) || sources.len() > max_sources {
        return Err(());
    }
    for (position, source) in sources.iter().enumerate() {
        if source.rank != (position + 1) as u32
            || source.page == 0
            || !source.score.is_finite()
            || source.paper_id.trim().is_empty()
            || source.filename.trim().is_empty()
            || source.title.trim().is_empty()
            || source.snippet.trim().is_empty()
            || source.snippet.chars().count() > 280
        {
            return Err(());
        }
    }
    Ok(sources)
}

fn invalid_chat_stream_event() -> Option<ChatStreamEvent> {
    Some(ChatStreamEvent::Error {
        message: "invalid chat stream payload".to_string(),
    })
}

fn chat_subquery_fields(value: &Value) -> Option<(String, u32, u32, String)> {
    let id = value.get("id")?.as_str()?.to_string();
    let index = u32::try_from(value.get("index")?.as_u64()?).ok()?;
    let total = u32::try_from(value.get("total")?.as_u64()?).ok()?;
    let query = value.get("query")?.as_str()?.to_string();
    if !(2..=MAX_CHAT_SUBQUERIES as u32).contains(&total)
        || index == 0
        || index > total
        || id != format!("subquery-{index}")
        || query.trim().is_empty()
    {
        return None;
    }
    Some((id, index, total, query))
}

fn chat_coverage_event(value: &Value) -> Option<ChatStreamEvent> {
    let Some(raw_status) = value.get("status").and_then(Value::as_str) else {
        return invalid_chat_stream_event();
    };
    let status = match raw_status {
        "reviewing" => ChatCoverageStatus::Reviewing,
        "complete" => ChatCoverageStatus::Complete,
        "followup" => ChatCoverageStatus::Followup,
        _ => return invalid_chat_stream_event(),
    };
    let Some(total) = value
        .get("total")
        .and_then(Value::as_u64)
        .and_then(|total| u32::try_from(total).ok())
    else {
        return invalid_chat_stream_event();
    };
    let Ok(subqueries) = serde_json::from_value::<Vec<ChatSubquery>>(
        value.get("subqueries").cloned().unwrap_or(Value::Null),
    ) else {
        return invalid_chat_stream_event();
    };
    let valid = match status {
        ChatCoverageStatus::Reviewing | ChatCoverageStatus::Complete => {
            (2..=MAX_CHAT_INITIAL_SUBQUERIES as u32).contains(&total) && subqueries.is_empty()
        }
        ChatCoverageStatus::Followup => {
            let followup_count = subqueries.len();
            if !(1..=2).contains(&followup_count)
                || !(3..=MAX_CHAT_SUBQUERIES as u32).contains(&total)
                || (total as usize) < followup_count + 2
            {
                false
            } else {
                let first_index = total - followup_count as u32 + 1;
                let mut normalized = Vec::with_capacity(followup_count);
                subqueries.iter().enumerate().all(|(offset, subquery)| {
                    let index = first_index + offset as u32;
                    let key = subquery.query.trim().to_lowercase();
                    let valid = subquery.id == format!("subquery-{index}")
                        && subquery.index == index
                        && !key.is_empty()
                        && !normalized.contains(&key);
                    normalized.push(key);
                    valid
                })
            }
        }
    };
    if !valid {
        return invalid_chat_stream_event();
    }
    Some(ChatStreamEvent::Coverage {
        status,
        total,
        subqueries,
    })
}

fn chat_done_event(value: &Value) -> Option<ChatStreamEvent> {
    if value.get("done").and_then(Value::as_bool) != Some(true) {
        return invalid_chat_stream_event();
    }
    let Some(reply) = value.get("reply").and_then(Value::as_str) else {
        return invalid_chat_stream_event();
    };
    let Some(chat_mode) = value.get("chat_mode").and_then(Value::as_str) else {
        return invalid_chat_stream_event();
    };
    let Some(indexed) = value.get("indexed").and_then(Value::as_bool) else {
        return invalid_chat_stream_event();
    };
    let Ok(sources) = chat_sources(value, MAX_CHAT_SOURCES, true) else {
        return Some(ChatStreamEvent::Error {
            message: "invalid chat source payload".to_string(),
        });
    };
    if reply.trim().is_empty() || chat_mode.trim().is_empty() {
        return invalid_chat_stream_event();
    }
    Some(ChatStreamEvent::Done {
        reply: reply.to_string(),
        chat_mode: chat_mode.to_string(),
        indexed,
        sources,
    })
}

fn chat_stream_event(value: &Value) -> Option<ChatStreamEvent> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return Some(ChatStreamEvent::Error {
            message: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        });
    }
    let event = match value.get("event") {
        Some(event) => match event.as_str() {
            Some(event) => Some(event),
            None => return invalid_chat_stream_event(),
        },
        None => None,
    };
    match event {
        Some("plan") => {
            let Ok(subqueries) = serde_json::from_value::<Vec<ChatSubquery>>(
                value.get("subqueries").cloned().unwrap_or(Value::Null),
            ) else {
                return invalid_chat_stream_event();
            };
            let mut normalized = Vec::with_capacity(subqueries.len());
            if !(2..=MAX_CHAT_INITIAL_SUBQUERIES).contains(&subqueries.len()) {
                return invalid_chat_stream_event();
            }
            for (position, subquery) in subqueries.iter().enumerate() {
                let index = (position + 1) as u32;
                let key = subquery.query.trim().to_lowercase();
                if subquery.id != format!("subquery-{index}")
                    || subquery.index != index
                    || key.is_empty()
                    || normalized.contains(&key)
                {
                    return invalid_chat_stream_event();
                }
                normalized.push(key);
            }
            Some(ChatStreamEvent::Plan { subqueries })
        }
        Some("subquery") => {
            let Some((id, index, total, query)) = chat_subquery_fields(value) else {
                return invalid_chat_stream_event();
            };
            Some(ChatStreamEvent::Subquery {
                id,
                index,
                total,
                query,
            })
        }
        Some("subanswer") => {
            let Some((id, index, total, query)) = chat_subquery_fields(value) else {
                return invalid_chat_stream_event();
            };
            let Some(answer) = value.get("answer").and_then(Value::as_str) else {
                return invalid_chat_stream_event();
            };
            let Ok(sources) = chat_sources(value, 5, false) else {
                return invalid_chat_stream_event();
            };
            if answer.trim().is_empty() {
                return invalid_chat_stream_event();
            }
            Some(ChatStreamEvent::Subanswer {
                id,
                index,
                total,
                query,
                answer: answer.to_string(),
                sources,
            })
        }
        Some("coverage") => chat_coverage_event(value),
        Some("delta") => match value.get("delta").and_then(Value::as_str) {
            Some(delta) if !delta.is_empty() => Some(ChatStreamEvent::Delta {
                delta: delta.to_string(),
            }),
            _ => invalid_chat_stream_event(),
        },
        Some("done") => chat_done_event(value),
        Some(_) => invalid_chat_stream_event(),
        None => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                Some(ChatStreamEvent::Delta {
                    delta: delta.to_string(),
                })
            } else if value.get("done").and_then(Value::as_bool) == Some(true) {
                chat_done_event(value)
            } else {
                None
            }
        }
    }
}

/// Determinate paper-indexing events forwarded over a Tauri `Channel`.
#[tauri::command]
pub(crate) async fn chat_stream(
    session_id: String,
    message: String,
    on_event: Channel<ChatStreamEvent>,
) -> Result<(), CommandError> {
    let root = project_root();
    run_blocking(move || {
        let request = serde_json::json!({
            "command": "chat",
            "session_id": session_id,
            "message": message,
            "stream": true,
        });
        let mut terminal_event_seen = false;
        let result = bridge::call_streaming(&root, request, |value| {
            if terminal_event_seen {
                return;
            }
            if let Some(event) = chat_stream_event(&value) {
                terminal_event_seen = matches!(
                    &event,
                    ChatStreamEvent::Done { .. } | ChatStreamEvent::Error { .. }
                );
                let _ = on_event.send(event);
            }
        });
        if !terminal_event_seen {
            let _ = on_event.send(ChatStreamEvent::Error {
                message: CHAT_TRANSPORT_ERROR.to_string(),
            });
        }
        result
    })
    .await
}

#[tauri::command]
pub(crate) async fn list_sessions() -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || bridge::call(&root, serde_json::json!({ "command": "sessions" }))).await
}

#[tauri::command]
pub(crate) async fn get_messages(session_id: String) -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || {
        bridge::call(
            &root,
            serde_json::json!({ "command": "messages", "session_id": session_id }),
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn search(
    query: String,
    limit: Option<u32>,
    session_id: Option<String>,
) -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || {
        let mut request = serde_json::json!({ "command": "search", "query": query });
        if let Some(limit) = limit {
            request["limit"] = serde_json::json!(limit);
        }
        if let Some(session_id) = session_id {
            request["session_id"] = serde_json::json!(session_id);
        }
        bridge::call(&root, request)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::test_support::run_with_fake_python;

    fn chat_source(rank: u32) -> Value {
        serde_json::json!({
            "rank": rank,
            "paper_id": format!("paper-{rank}"),
            "chunk_index": rank - 1,
            "filename": format!("paper-{rank}.pdf"),
            "title": format!("Paper {rank}"),
            "page": rank,
            "score": 1.0 / f64::from(rank),
            "snippet": format!("evidence {rank}"),
        })
    }

    #[test]
    fn maps_and_serializes_chat_done_sources() {
        let event = chat_stream_event(&serde_json::json!({
            "ok": true,
            "done": true,
            "reply": "answer",
            "chat_mode": "mock",
            "indexed": true,
            "sources": [{
                "rank": 1,
                "paper_id": "paper-a",
                "chunk_index": 2,
                "filename": "paper-a.pdf",
                "title": "Paper A",
                "page": 3,
                "score": 0.75,
                "snippet": "concise evidence",
            }],
        }))
        .unwrap();

        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "done");
        assert_eq!(value["chatMode"], "mock");
        assert_eq!(value["sources"][0]["paper_id"], "paper-a");
        assert_eq!(value["sources"][0]["chunk_index"], 2);
    }

    #[test]
    fn maps_and_serializes_ordered_multihop_chat_stages() {
        let plan = chat_stream_event(&serde_json::json!({
            "event": "plan",
            "subqueries": [
                {"id": "subquery-1", "index": 1, "query": "alpha"},
                {"id": "subquery-2", "index": 2, "query": "beta"},
            ],
        }))
        .unwrap();
        let started = chat_stream_event(&serde_json::json!({
            "event": "subquery", "id": "subquery-1", "index": 1,
            "total": 2, "query": "alpha",
        }))
        .unwrap();
        let answer = chat_stream_event(&serde_json::json!({
            "event": "subanswer", "id": "subquery-1", "index": 1,
            "total": 2, "query": "alpha", "answer": "summary",
            "sources": [chat_source(1)],
        }))
        .unwrap();
        let coverage = chat_stream_event(&serde_json::json!({
            "event": "coverage", "status": "followup", "total": 4,
            "subqueries": [
                {"id": "subquery-3", "index": 3, "query": "gamma"},
                {"id": "subquery-4", "index": 4, "query": "delta"},
            ],
        }))
        .unwrap();
        let delta = chat_stream_event(&serde_json::json!({
            "event": "delta", "delta": "final text",
        }))
        .unwrap();

        let values: Vec<Value> = [plan, started, answer, coverage, delta]
            .into_iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        assert_eq!(
            values
                .iter()
                .map(|value| value["event"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["plan", "subquery", "subanswer", "coverage", "delta"]
        );
        assert_eq!(values[0]["subqueries"][1]["id"], "subquery-2");
        assert_eq!(values[1]["index"], 1);
        assert_eq!(values[2]["answer"], "summary");
        assert_eq!(values[3]["status"], "followup");
        assert_eq!(values[3]["subqueries"][1]["id"], "subquery-4");
        assert_eq!(values[4]["delta"], "final text");
    }

    #[test]
    fn malformed_multihop_payloads_map_to_safe_errors() {
        for payload in [
            serde_json::json!({
                "event": "plan",
                "subqueries": [
                    {"id": "wrong", "index": 1, "query": "alpha"},
                    {"id": "subquery-2", "index": 2, "query": "beta"},
                ],
            }),
            serde_json::json!({
                "event": "subanswer", "id": "subquery-1", "index": 1,
                "total": 2, "query": "alpha", "answer": "summary",
            }),
            serde_json::json!({
                "event": "coverage", "status": "followup", "total": 3,
                "subqueries": [{"id": "subquery-4", "index": 4, "query": "gap"}],
            }),
            serde_json::json!({"event": "private_reasoning", "text": "hidden"}),
        ] {
            assert_eq!(
                chat_stream_event(&payload),
                Some(ChatStreamEvent::Error {
                    message: "invalid chat stream payload".to_string(),
                })
            );
        }
    }

    #[test]
    fn accepts_aggregate_done_sources_and_legacy_delta() {
        let sources: Vec<Value> = (1..=35).map(chat_source).collect();
        let done = chat_stream_event(&serde_json::json!({
            "ok": true,
            "done": true,
            "reply": "answer",
            "chat_mode": "mock",
            "indexed": true,
            "sources": sources,
        }))
        .unwrap();

        assert!(matches!(
            done,
            ChatStreamEvent::Done { sources, .. } if sources.len() == 35
        ));
        assert!(matches!(
            chat_stream_event(&serde_json::json!({
                "ok": true, "done": true, "reply": "answer", "chat_mode": "mock",
                "indexed": true, "sources": (1..=36).map(chat_source).collect::<Vec<_>>(),
            })),
            Some(ChatStreamEvent::Error { .. })
        ));
        assert_eq!(
            chat_stream_event(&serde_json::json!({"delta": "legacy"})),
            Some(ChatStreamEvent::Delta {
                delta: "legacy".to_string(),
            })
        );
    }

    #[test]
    fn accepts_source_less_legacy_done_completion() {
        let done = chat_stream_event(&serde_json::json!({
            "ok": true,
            "done": true,
            "reply": "answer",
            "chat_mode": "mock",
            "indexed": false,
            "sources": [],
        }))
        .unwrap();

        assert!(matches!(done, ChatStreamEvent::Done { sources, .. } if sources.is_empty()));
    }

    #[test]
    fn maps_malformed_chat_sources_to_error() {
        let event = chat_stream_event(&serde_json::json!({
            "ok": true,
            "done": true,
            "reply": "answer",
            "chat_mode": "mock",
            "indexed": false,
            "sources": [{"rank": 2}],
        }));

        assert_eq!(
            event,
            Some(ChatStreamEvent::Error {
                message: "invalid chat source payload".to_string(),
            })
        );
    }

    #[test]
    fn maps_chat_output_limit_failure_to_error_before_any_delta() {
        let event = chat_stream_event(&serde_json::json!({
            "ok": false,
            "delta": "partial",
            "error": "Chat response was incomplete; shorten the request and try again",
            "error_type": "AgentIncompleteResponseError",
        }));

        assert_eq!(
            event,
            Some(ChatStreamEvent::Error {
                message: "Chat response was incomplete; shorten the request and try again"
                    .to_string(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn chat_bridge_failure_emits_one_sanitized_terminal_error() {
        let (result, events) = run_with_fake_python(
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"plan\",\"subqueries\":[{\"id\":\"subquery-1\",\"index\":1,\"query\":\"alpha\"},{\"id\":\"subquery-2\",\"index\":2,\"query\":\"beta\"}]}'\nexit 17\n",
            |channel| tauri::async_runtime::block_on(chat_stream("session".to_string(), "question".to_string(), channel)),
        );

        result.expect_err("non-zero bridge process must fail");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "plan");
        assert_eq!(events[1]["event"], "error");
        assert_eq!(events[1]["message"], CHAT_TRANSPORT_ERROR);
    }
}
