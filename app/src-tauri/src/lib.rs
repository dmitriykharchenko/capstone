//! Tauri backend for the capstone research assistant.
//!
//! Papers listing and PDF serving are handled natively (filesystem with a
//! safe-id path-traversal guard). Chat, sessions, messages, search, and health
//! are delegated to the Python CLI bridge (`python -m src.cli`) by piping one
//! JSON request on stdin and parsing one JSON response on stdout.

mod bridge;
mod error;
mod evaluation;
mod graph;
mod orchestration;
mod orchestration_bridge;
mod orchestration_wire;
mod papers;

use error::CommandError;
use evaluation::{
    evaluation_stream_event, evaluation_transport_error_event, evaluation_transport_error_message,
    EvaluationRequestCase, EvaluationRetrievalType, EvaluationStreamEvent,
};
use papers::{PaperInfo, PaperRepository, PdfData};
use serde_json::Value;
use tauri::ipc::Channel;

/// Streamed chat events forwarded to the frontend over a Tauri `Channel`.
///
/// Serialized with an internal `event` tag so TypeScript can discriminate every
/// public multihop stage while retaining the legacy `delta` / `done` contract.
const MAX_CHAT_SOURCES: usize = 35;
const MAX_CHAT_INITIAL_SUBQUERIES: usize = 5;
const MAX_CHAT_SUBQUERIES: usize = 7;
const CHAT_TRANSPORT_ERROR: &str = "Chat transport failed. Check backend logs and retry.";

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct ChatSubquery {
    id: String,
    index: u32,
    query: String,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct ChatSource {
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
enum ChatCoverageStatus {
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
enum ChatStreamEvent {
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
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum PaperIndexStreamEvent {
    Start {
        completed: u32,
        total: u32,
    },
    Progress {
        completed: u32,
        total: u32,
        paper_id: String,
        filename: String,
        status: String,
        chunk_count: u32,
        error: Option<String>,
    },
    Done {
        completed: u32,
        total: u32,
        indexed: u32,
        empty: u32,
        failed: u32,
    },
    Error {
        message: String,
    },
}

fn paper_index_stream_event(value: &Value) -> PaperIndexStreamEvent {
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|number| u32::try_from(number).ok())
    };
    let invalid = || PaperIndexStreamEvent::Error {
        message: "invalid paper indexing progress event".to_string(),
    };
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return PaperIndexStreamEvent::Error {
            message: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        };
    }
    match value.get("event").and_then(Value::as_str) {
        Some("start") => match (field("completed"), field("total")) {
            (Some(completed), Some(total)) => PaperIndexStreamEvent::Start { completed, total },
            _ => invalid(),
        },
        Some("progress") => match (
            field("completed"),
            field("total"),
            value.get("paper_id").and_then(Value::as_str),
            value.get("filename").and_then(Value::as_str),
            value.get("status").and_then(Value::as_str),
            field("chunk_count"),
        ) {
            (
                Some(completed),
                Some(total),
                Some(paper_id),
                Some(filename),
                Some(status),
                Some(chunk_count),
            ) => PaperIndexStreamEvent::Progress {
                completed,
                total,
                paper_id: paper_id.to_string(),
                filename: filename.to_string(),
                status: status.to_string(),
                chunk_count,
                error: value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            _ => invalid(),
        },
        Some("done") => match (
            field("completed"),
            field("total"),
            field("indexed"),
            field("empty"),
            field("failed"),
        ) {
            (Some(completed), Some(total), Some(indexed), Some(empty), Some(failed)) => {
                PaperIndexStreamEvent::Done {
                    completed,
                    total,
                    indexed,
                    empty,
                    failed,
                }
            }
            _ => invalid(),
        },
        _ => invalid(),
    }
}

/// Resolve the repository root that contains the Python `src` package.
///
/// In development the Tauri process runs from `app/src-tauri`, so the root is
/// two levels up. An explicit `CAPSTONE_PROJECT_ROOT` override wins when set.
fn project_root() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("CAPSTONE_PROJECT_ROOT") {
        if !explicit.trim().is_empty() {
            return std::path::PathBuf::from(explicit);
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest)
}

fn repository() -> PaperRepository {
    PaperRepository::new(papers::papers_directory(&project_root()))
}

/// Run a blocking command body on Tauri's blocking thread pool so a slow Python
/// subprocess or filesystem read never stalls the UI event loop. Every command
/// stays `async` and awaits the result, so its invoke promise (and any streaming
/// `Channel`) settles only once the work finishes.
async fn run_blocking<T, F>(work: F) -> Result<T, CommandError>
where
    F: FnOnce() -> Result<T, CommandError> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| CommandError::Bridge(format!("background task failed: {error}")))?
}

#[tauri::command]
async fn list_papers() -> Result<Vec<PaperInfo>, CommandError> {
    run_blocking(|| repository().list_papers()).await
}

#[tauri::command]
async fn get_paper(paper_id: String) -> Result<PaperInfo, CommandError> {
    run_blocking(move || repository().get_paper(&paper_id)).await
}

#[tauri::command]
async fn read_pdf(paper_id: String) -> Result<PdfData, CommandError> {
    run_blocking(move || repository().read_pdf(&paper_id)).await
}

#[tauri::command]
async fn health() -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || bridge::call(&root, serde_json::json!({ "command": "health" }))).await
}

#[tauri::command]
async fn chat(session_id: String, message: String) -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || {
        bridge::call(
            &root,
            serde_json::json!({
                "command": "chat",
                "session_id": session_id,
                "message": message,
            }),
        )
    })
    .await
}

#[tauri::command]
async fn chat_stream(
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
async fn list_sessions() -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || bridge::call(&root, serde_json::json!({ "command": "sessions" }))).await
}

#[tauri::command]
async fn get_messages(session_id: String) -> Result<Value, CommandError> {
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
async fn search(
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

#[tauri::command]
async fn search_papers(query: String, limit: Option<u32>) -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || {
        let mut request = serde_json::json!({ "command": "search_papers", "query": query });
        if let Some(limit) = limit {
            request["limit"] = serde_json::json!(limit);
        }
        bridge::call(&root, request)
    })
    .await
}

#[tauri::command]
async fn graph_neighborhood(
    node_id: String,
    max_depth: Option<u32>,
) -> Result<Value, CommandError> {
    let request = graph::neighborhood_request(node_id, max_depth)?;
    let root = project_root();
    run_blocking(move || bridge::call(&root, request)).await
}

#[tauri::command]
async fn index_papers() -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || bridge::call(&root, serde_json::json!({ "command": "index_papers" }))).await
}

#[tauri::command]
async fn index_papers_stream(
    on_event: Channel<PaperIndexStreamEvent>,
) -> Result<(), CommandError> {
    let root = project_root();
    run_blocking(move || {
        let request = serde_json::json!({ "command": "index_papers", "stream": true });
        let mut sent_error = false;
        let result = bridge::call_streaming(&root, request, |value| {
            let event = paper_index_stream_event(&value);
            sent_error |= matches!(event, PaperIndexStreamEvent::Error { .. });
            let _ = on_event.send(event);
        });
        if let Err(error) = result {
            if !sent_error {
                let _ = on_event.send(PaperIndexStreamEvent::Error {
                    message: error.to_string(),
                });
            }
            return Err(error);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
async fn evaluate_stream(
    retrieval_type: EvaluationRetrievalType,
    test_cases: Vec<EvaluationRequestCase>,
    on_event: Channel<EvaluationStreamEvent>,
) -> Result<(), CommandError> {
    if test_cases.is_empty() || test_cases.iter().any(|case| !case.validate()) {
        return Err(CommandError::Bridge(
            "test cases must be a non-empty array of valid cases".to_string(),
        ));
    }
    let root = project_root();
    run_blocking(move || {
        let request = serde_json::json!({
            "command": "evaluate",
            "stream": true,
            "retrieval_type": retrieval_type,
            "test_cases": test_cases,
        });
        let mut terminal_event_seen = false;
        let result = bridge::call_streaming(&root, request, |value| {
            let event = evaluation_stream_event(value);
            terminal_event_seen |= matches!(
                event,
                EvaluationStreamEvent::Complete { .. } | EvaluationStreamEvent::Error { .. }
            );
            let _ = on_event.send(event);
        });
        if result.is_err() {
            if !terminal_event_seen {
                let _ = on_event.send(evaluation_transport_error_event());
            }
            return Err(CommandError::Bridge(
                evaluation_transport_error_message().to_string(),
            ));
        }
        if !terminal_event_seen {
            let message = "evaluation stream ended before terminal completion".to_string();
            let _ = on_event.send(EvaluationStreamEvent::Error {
                message: message.clone(),
            });
            return Err(CommandError::InvalidResponse(message));
        }
        Ok(())
    })
    .await
}

/// Build and run the Tauri application, registering every command handler.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list_papers,
            get_paper,
            read_pdf,
            health,
            chat,
            chat_stream,
            orchestration::orchestration_start,
            orchestration::orchestration_resume,
            orchestration::orchestration_pending,
            orchestration::orchestration_discard,
            orchestration::orchestration_trace,
            list_sessions,
            get_messages,
            search,
            search_papers,
            graph_neighborhood,
            index_papers,
            index_papers_stream,
            evaluate_stream,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    static PYTHON_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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

    #[cfg(unix)]
    fn executable_script(contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capstone-index-stream-{suffix}.sh"));
        fs::write(&path, contents).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn maps_and_serializes_paper_progress_event() {
        let event = paper_index_stream_event(&serde_json::json!({
            "event": "progress",
            "completed": 1,
            "total": 2,
            "paper_id": "paper-a",
            "filename": "paper-a.pdf",
            "status": "indexed",
            "chunk_count": 3,
            "error": null,
        }));

        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "progress");
        assert_eq!(value["paperId"], "paper-a");
        assert_eq!(value["chunkCount"], 3);
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
        let _lock = PYTHON_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let script = executable_script(
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"plan\",\"subqueries\":[{\"id\":\"subquery-1\",\"index\":1,\"query\":\"alpha\"},{\"id\":\"subquery-2\",\"index\":2,\"query\":\"beta\"}]}'\nexit 17\n",
        );
        let previous = std::env::var_os("CAPSTONE_PYTHON");
        std::env::set_var("CAPSTONE_PYTHON", &script);
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let received_clone = Arc::clone(&received);
        let channel = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                received_clone
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&json).unwrap());
            }
            Ok(())
        });

        let result = tauri::async_runtime::block_on(chat_stream(
            "session".to_string(),
            "question".to_string(),
            channel,
        ));
        match previous {
            Some(value) => std::env::set_var("CAPSTONE_PYTHON", value),
            None => std::env::remove_var("CAPSTONE_PYTHON"),
        }
        let _ = fs::remove_file(script);

        result.expect_err("non-zero bridge process must fail");
        let events = received.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "plan");
        assert_eq!(events[1]["event"], "error");
        assert_eq!(events[1]["message"], CHAT_TRANSPORT_ERROR);
    }

    #[test]
    fn maps_terminal_summary_and_process_error() {
        let done = paper_index_stream_event(&serde_json::json!({
            "event": "done", "completed": 4, "total": 4,
            "indexed": 2, "empty": 1, "failed": 1,
        }));
        assert_eq!(
            done,
            PaperIndexStreamEvent::Done {
                completed: 4,
                total: 4,
                indexed: 2,
                empty: 1,
                failed: 1,
            }
        );

        let error = paper_index_stream_event(&serde_json::json!({
            "ok": false, "error": "python failed",
        }));
        assert_eq!(
            error,
            PaperIndexStreamEvent::Error {
                message: "python failed".to_string(),
            }
        );
    }

    #[test]
    fn maps_malformed_progress_event_to_error() {
        let malformed = paper_index_stream_event(&serde_json::json!({
            "event": "progress",
            "completed": 1,
            "total": 2,
            "paper_id": "paper-a",
        }));

        assert_eq!(
            malformed,
            PaperIndexStreamEvent::Error {
                message: "invalid paper indexing progress event".to_string(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn forwards_nonzero_bridge_failure_to_channel_error() {
        let _lock = PYTHON_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let script = executable_script(
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"start\",\"completed\":0,\"total\":0}'\nprintf '%s\\n' 'process failed' >&2\nexit 17\n",
        );
        let previous = std::env::var_os("CAPSTONE_PYTHON");
        std::env::set_var("CAPSTONE_PYTHON", &script);
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let received_clone = Arc::clone(&received);
        let channel = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                received_clone
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&json).unwrap());
            }
            Ok(())
        });

        let result = tauri::async_runtime::block_on(index_papers_stream(channel));
        match previous {
            Some(value) => std::env::set_var("CAPSTONE_PYTHON", value),
            None => std::env::remove_var("CAPSTONE_PYTHON"),
        }
        let _ = fs::remove_file(script);

        let error = result.expect_err("non-zero bridge process must fail");
        assert!(error.to_string().contains("python exited"));
        let events = received.lock().unwrap();
        assert_eq!(events[0]["event"], "start");
        assert_eq!(events[1]["event"], "error");
        assert!(events[1]["message"]
            .as_str()
            .unwrap()
            .contains("python exited"));
    }

    #[cfg(unix)]
    #[test]
    fn evaluation_persistence_outcomes_have_one_terminal_channel_frame() {
        let _lock = PYTHON_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let input = serde_json::json!({"question": "q", "grading_notes": "g", "sources": []});
        let progress = [
            serde_json::json!({"event": "run_start", "retrieval_type": "hybrid", "total": 1}),
            serde_json::json!({
                "event": "case_start", "case_index": 1, "total": 1,
                "retrieval_type": "hybrid", "input": input,
            }),
            serde_json::json!({
                "event": "case_failure", "completed": 1, "total": 1,
                "result": {
                    "case_index": 1, "input": input, "retrieval_strategy": "hybrid",
                    "retrieved_chunks": [], "generated_answer": null, "judge_scores": null,
                    "error": {"stage": "judge", "message": "Judge evaluation failed for this case. Check backend configuration and retry."},
                },
            }),
        ];
        let complete = serde_json::json!({
            "event": "complete", "retrieval_type": "hybrid", "completed": 1,
            "total": 1, "succeeded": 0, "failed": 1,
            "aggregate_scores": {
                "context_precision": null, "context_recall": null,
                "relevant_chunk_ranking": null, "context_utilization": null,
                "semantic_similarity": null, "completeness": null, "hallucination_rate": null,
            },
        });
        // Mirror the Python persistence boundary: success completes; a failed
        // write emits only the sanitized error and exits nonzero.
        for (terminal, exit_code, terminal_name) in [
            (complete, 0, "complete"),
            (
                serde_json::json!({"ok": false, "error": evaluation_transport_error_message()}),
                1,
                "error",
            ),
        ] {
            let frames = progress.iter().chain(std::iter::once(&terminal));
            let mut contents = String::from("#!/bin/sh\n");
            for frame in frames {
                contents.push_str(&format!("printf '%s\\n' '{}'\n", frame));
            }
            contents.push_str(&format!("exit {exit_code}\n"));
            let script = executable_script(&contents);
            let previous = std::env::var_os("CAPSTONE_PYTHON");
            std::env::set_var("CAPSTONE_PYTHON", &script);
            let received = Arc::new(Mutex::new(Vec::<Value>::new()));
            let received_clone = Arc::clone(&received);
            let channel = Channel::new(move |body| {
                if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                    received_clone
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&json).unwrap());
                }
                Ok(())
            });

            let result = tauri::async_runtime::block_on(evaluate_stream(
                EvaluationRetrievalType::Hybrid,
                vec![serde_json::from_value(input.clone()).unwrap()],
                channel,
            ));
            match previous {
                Some(value) => std::env::set_var("CAPSTONE_PYTHON", value),
                None => std::env::remove_var("CAPSTONE_PYTHON"),
            }
            let _ = fs::remove_file(script);

            assert_eq!(result.is_ok(), exit_code == 0);
            let events = received.lock().unwrap();
            let names: Vec<_> = events
                .iter()
                .map(|event| event["event"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                ["runStart", "caseStart", "caseFailure", terminal_name]
            );
            assert_eq!(events[1]["input"], input);
            assert_eq!(events[2]["result"], progress[2]["result"]);
            assert_eq!(
                events.last().unwrap(),
                &serde_json::to_value(evaluation_stream_event(terminal)).unwrap()
            );
            assert_eq!(
                names
                    .iter()
                    .filter(|name| matches!(**name, "complete" | "error"))
                    .count(),
                1
            );
            if let Err(error) = result {
                assert_eq!(
                    error.to_string(),
                    format!(
                        "python bridge failed: {}",
                        evaluation_transport_error_message()
                    )
                );
            }
        }
    }
}
