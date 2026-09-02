//! Tauri backend for the capstone research assistant.
//!
//! Papers listing and PDF serving are handled natively (filesystem with a
//! safe-id path-traversal guard). Chat, sessions, messages, search, and health
//! are delegated to the Python CLI bridge (`python -m src.cli`) by piping one
//! JSON request on stdin and parsing one JSON response on stdout.

mod bridge;
mod error;
mod papers;

use error::CommandError;
use papers::{PaperInfo, PaperRepository, PdfData};
use serde_json::Value;
use tauri::ipc::Channel;

/// Streamed chat events forwarded to the frontend over a Tauri `Channel`.
///
/// Serialized with an internal `event` tag (`delta` / `done` / `error`) so the
/// TypeScript side can discriminate on `event`.
#[derive(Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
enum ChatStreamEvent {
    Delta {
        delta: String,
    },
    Done {
        reply: String,
        chat_mode: String,
        indexed: bool,
    },
    Error {
        message: String,
    },
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

#[tauri::command]
fn list_papers() -> Result<Vec<PaperInfo>, CommandError> {
    repository().list_papers()
}

#[tauri::command]
fn get_paper(paper_id: String) -> Result<PaperInfo, CommandError> {
    repository().get_paper(&paper_id)
}

#[tauri::command]
fn read_pdf(paper_id: String) -> Result<PdfData, CommandError> {
    repository().read_pdf(&paper_id)
}

#[tauri::command]
fn health() -> Result<Value, CommandError> {
    bridge::call(&project_root(), serde_json::json!({ "command": "health" }))
}

#[tauri::command]
fn chat(session_id: String, message: String) -> Result<Value, CommandError> {
    bridge::call(
        &project_root(),
        serde_json::json!({
            "command": "chat",
            "session_id": session_id,
            "message": message,
        }),
    )
}

#[tauri::command]
fn chat_stream(
    session_id: String,
    message: String,
    on_event: Channel<ChatStreamEvent>,
) -> Result<(), CommandError> {
    let request = serde_json::json!({
        "command": "chat",
        "session_id": session_id,
        "message": message,
        "stream": true,
    });
    bridge::call_streaming(&project_root(), request, |value| {
        let event = if let Some(delta) = value.get("delta").and_then(Value::as_str) {
            ChatStreamEvent::Delta {
                delta: delta.to_string(),
            }
        } else if value.get("ok").and_then(Value::as_bool) == Some(false) {
            ChatStreamEvent::Error {
                message: value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            }
        } else if value.get("done").and_then(Value::as_bool) == Some(true) {
            ChatStreamEvent::Done {
                reply: value
                    .get("reply")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                chat_mode: value
                    .get("chat_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                indexed: value
                    .get("indexed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        } else {
            return;
        };
        let _ = on_event.send(event);
    })
}

#[tauri::command]
fn list_sessions() -> Result<Value, CommandError> {
    bridge::call(
        &project_root(),
        serde_json::json!({ "command": "sessions" }),
    )
}

#[tauri::command]
fn get_messages(session_id: String) -> Result<Value, CommandError> {
    bridge::call(
        &project_root(),
        serde_json::json!({ "command": "messages", "session_id": session_id }),
    )
}

#[tauri::command]
fn search(
    query: String,
    limit: Option<u32>,
    session_id: Option<String>,
) -> Result<Value, CommandError> {
    let mut request = serde_json::json!({ "command": "search", "query": query });
    if let Some(limit) = limit {
        request["limit"] = serde_json::json!(limit);
    }
    if let Some(session_id) = session_id {
        request["session_id"] = serde_json::json!(session_id);
    }
    bridge::call(&project_root(), request)
}

#[tauri::command]
async fn search_papers(query: String, limit: Option<u32>) -> Result<Value, CommandError> {
    let root = project_root();
    // Query embedding + Chroma lookup run through the blocking Python subprocess;
    // run them on the blocking thread pool so the UI stays responsive.
    tauri::async_runtime::spawn_blocking(move || {
        let mut request = serde_json::json!({ "command": "search_papers", "query": query });
        if let Some(limit) = limit {
            request["limit"] = serde_json::json!(limit);
        }
        bridge::call(&root, request)
    })
    .await
    .map_err(|error| CommandError::Bridge(format!("search task failed: {error}")))?
}

#[tauri::command]
fn index_papers() -> Result<Value, CommandError> {
    bridge::call(
        &project_root(),
        serde_json::json!({ "command": "index_papers" }),
    )
}

#[tauri::command]
async fn index_papers_stream(
    on_event: Channel<PaperIndexStreamEvent>,
) -> Result<(), CommandError> {
    let root = project_root();
    // Indexing spawns Python and blocks on its stdout for the whole run (minutes
    // for a large corpus). Run it on the blocking thread pool instead of the main
    // thread so the UI stays responsive; await it so the invoke promise (and thus
    // the frontend event channel) settles only after the terminal event.
    tauri::async_runtime::spawn_blocking(move || {
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
    .map_err(|error| CommandError::Bridge(format!("indexing task failed: {error}")))?
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
            list_sessions,
            get_messages,
            search,
            search_papers,
            index_papers,
            index_papers_stream,
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
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
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

        let result = index_papers_stream(channel);
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
}
