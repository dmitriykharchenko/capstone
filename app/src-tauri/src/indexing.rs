//! Paper indexing progress, index status and index reset over the Python bridge.

use serde_json::Value;
use tauri::{ipc::Channel, State};

use crate::error::CommandError;
use crate::library::LibraryState;
use crate::{bridge, project_root, run_blocking};

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum PaperIndexStreamEvent {
    Start {
        completed: u32,
        total: u32,
    },
    Started {
        completed: u32,
        total: u32,
        paper_id: String,
        filename: String,
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
        skipped: u32,
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
        Some("started") => match (
            field("completed"),
            field("total"),
            value.get("paper_id").and_then(Value::as_str),
            value.get("filename").and_then(Value::as_str),
        ) {
            (Some(completed), Some(total), Some(paper_id), Some(filename)) => {
                PaperIndexStreamEvent::Started {
                    completed,
                    total,
                    paper_id: paper_id.to_string(),
                    filename: filename.to_string(),
                }
            }
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
                    // Absent from older backends; default to zero skipped.
                    skipped: field("skipped").unwrap_or(0),
                }
            }
            _ => invalid(),
        },
        _ => invalid(),
    }
}

#[tauri::command]
pub(crate) async fn index_papers_stream(
    on_event: Channel<PaperIndexStreamEvent>,
    state: State<'_, LibraryState>,
) -> Result<(), CommandError> {
    index_papers_stream_with_state(on_event, &state).await
}

async fn index_papers_stream_with_state(
    on_event: Channel<PaperIndexStreamEvent>,
    state: &LibraryState,
) -> Result<(), CommandError> {
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        let request = serde_json::json!({ "command": "index_papers", "stream": true });
        let mut sent_error = false;
        let result =
            bridge::call_streaming(&root, operation.library_directory(), request, |value| {
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
pub(crate) async fn paper_statuses(state: State<'_, LibraryState>) -> Result<Value, CommandError> {
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        bridge::call(
            &root,
            operation.library_directory(),
            serde_json::json!({ "command": "paper_statuses" }),
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn bm25_status(state: State<'_, LibraryState>) -> Result<Value, CommandError> {
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        bridge::call(
            &root,
            operation.library_directory(),
            serde_json::json!({ "command": "bm25_status" }),
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn reset_all_indices(
    state: State<'_, LibraryState>,
) -> Result<Value, CommandError> {
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        bridge::call(
            &root,
            operation.library_directory(),
            serde_json::json!({ "command": "reset_all_indices" }),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::test_support::{run_with_fake_python, test_library_state};

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
    fn maps_and_serializes_paper_started_event() {
        let event = paper_index_stream_event(&serde_json::json!({
            "event": "started",
            "completed": 1,
            "total": 3,
            "paper_id": "paper-b",
            "filename": "paper-b.pdf",
        }));

        assert_eq!(
            event,
            PaperIndexStreamEvent::Started {
                completed: 1,
                total: 3,
                paper_id: "paper-b".to_string(),
                filename: "paper-b.pdf".to_string(),
            }
        );

        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "started");
        assert_eq!(value["paperId"], "paper-b");
        assert_eq!(value["filename"], "paper-b.pdf");
    }

    #[test]
    fn maps_terminal_summary_and_process_error() {
        let done = paper_index_stream_event(&serde_json::json!({
            "event": "done", "completed": 4, "total": 4,
            "indexed": 2, "empty": 1, "failed": 1, "skipped": 3,
        }));
        assert_eq!(
            done,
            PaperIndexStreamEvent::Done {
                completed: 4,
                total: 4,
                indexed: 2,
                empty: 1,
                failed: 1,
                skipped: 3,
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
        let state = test_library_state();
        let (result, events) = run_with_fake_python(
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"start\",\"completed\":0,\"total\":0}'\nprintf '%s\\n' 'process failed' >&2\nexit 17\n",
            |channel| {
                tauri::async_runtime::block_on(index_papers_stream_with_state(channel, &state))
            },
        );

        let error = result.expect_err("non-zero bridge process must fail");
        assert!(error.to_string().contains("python exited"));
        assert_eq!(events[0]["event"], "start");
        assert_eq!(events[1]["event"], "error");
        assert!(events[1]["message"]
            .as_str()
            .unwrap()
            .contains("python exited"));
    }
}
