use crate::error::CommandError;
mod bridge;
mod wire;

use serde_json::{json, Value};
use tauri::{ipc::Channel, State};
use wire::{validate_type, Validator, ERROR};

use crate::library::LibraryState;

fn failure() -> CommandError {
    CommandError::InvalidResponse(ERROR.into())
}

fn forward<F>(
    request: Value,
    mut send: F,
    call: impl FnOnce(
        &Value,
        &mut dyn FnMut(Value) -> Result<(), &'static str>,
    ) -> Result<(), &'static str>,
) -> Result<(), CommandError>
where
    F: FnMut(Value) -> Result<(), &'static str>,
{
    let mut validator = Validator::new(
        request["session_id"].as_str().ok_or_else(failure)?,
        request["run_id"].as_str().ok_or_else(failure)?,
        request["turn_id"].as_str().ok_or_else(failure)?,
    )
    .map_err(|_| failure())?;
    let mut terminal = None;
    let mut channel_failed = false;
    let result = call(&request, &mut |value| {
        validator.accept(&value)?;
        if validator.terminal {
            terminal = Some(value);
        } else if send(value).is_err() {
            channel_failed = true;
            return Err(ERROR);
        }
        Ok(())
    });
    // Hold the terminal until successful EOF: malformed/duplicate/post-terminal
    // input becomes one sanitized error instead of a false successful completion.
    if channel_failed {
        return Err(failure());
    }
    if result.is_err() || terminal.is_none() {
        let _ = send(validator.error());
        return Err(failure());
    }
    send(terminal.unwrap()).map_err(|_| failure())
}

async fn stream(
    request: Value,
    on_event: Channel<Value>,
    state: &LibraryState,
) -> Result<(), CommandError> {
    if let Some(message) = request.get("message") {
        validate_type(message, "query").map_err(|_| failure())?;
    }
    let operation = state.begin_operation()?;
    let root = crate::project_root();
    crate::run_blocking(move || {
        forward(
            request,
            |value| on_event.send(value).map_err(|_| ERROR),
            |request, callback| {
                bridge::call(&root, operation.library_directory(), request, callback)
            },
        )
    })
    .await
}

#[tauri::command]
pub async fn orchestration_start(
    session_id: String,
    run_id: String,
    turn_id: String,
    message: String,
    on_event: Channel<Value>,
    state: State<'_, LibraryState>,
) -> Result<(), CommandError> {
    stream(json!({"command":"orchestration_start","session_id":session_id,"run_id":run_id,"turn_id":turn_id,"message":message}), on_event, &state).await
}

#[tauri::command]
pub async fn orchestration_resume(
    session_id: String,
    run_id: String,
    turn_id: String,
    on_event: Channel<Value>,
    state: State<'_, LibraryState>,
) -> Result<(), CommandError> {
    stream(json!({"command":"orchestration_resume","session_id":session_id,"run_id":run_id,"turn_id":turn_id}), on_event, &state).await
}

async fn control(request: Value, state: &LibraryState) -> Result<Value, CommandError> {
    for key in ["session_id", "run_id", "turn_id"] {
        if let Some(value) = request.get(key) {
            validate_type(value, "id").map_err(|_| failure())?;
        }
    }
    if let Some(value) = request.get("exchange_id") {
        validate_type(value, "exchange").map_err(|_| failure())?;
    }
    let operation = state.begin_operation()?;
    let root = crate::project_root();
    crate::run_blocking(move || {
        let mut result = None;
        bridge::call(&root, operation.library_directory(), &request, |value| {
            if result.is_some() {
                return Err(ERROR);
            }
            let key = if request["command"] == "orchestration_pending" {
                "pending"
            } else {
                "snapshot"
            };
            if value.as_object().ok_or(ERROR)?.len() != 1 {
                return Err(ERROR);
            }
            let snapshot = value.get(key).ok_or(ERROR)?;
            if snapshot.is_null() {
                if key != "pending" {
                    return Err(ERROR);
                }
            } else {
                let mut validator = Validator::new(
                    request["session_id"].as_str().unwrap(),
                    snapshot["run_id"].as_str().ok_or(ERROR)?,
                    snapshot["turn_id"].as_str().ok_or(ERROR)?,
                )?;
                validator.accept(snapshot)?;
                if snapshot["event"] != "snapshot" {
                    return Err(ERROR);
                }
                for id in ["run_id", "turn_id"] {
                    if let Some(expected) = request.get(id) {
                        if &snapshot[id] != expected {
                            return Err(ERROR);
                        }
                    }
                }
                if request["command"] == "orchestration_trace"
                    && snapshot["snapshot"]["exchange_id"] != request["exchange_id"]
                {
                    return Err(ERROR);
                }
            }
            result = Some(value);
            Ok(())
        })
        .map_err(|_| failure())?;
        result.ok_or_else(failure)
    })
    .await
}

#[tauri::command]
pub async fn orchestration_pending(
    session_id: String,
    state: State<'_, LibraryState>,
) -> Result<Value, CommandError> {
    control(
        json!({"command":"orchestration_pending","session_id":session_id}),
        &state,
    )
    .await
}
#[tauri::command]
pub async fn orchestration_discard(
    session_id: String,
    run_id: String,
    turn_id: String,
    state: State<'_, LibraryState>,
) -> Result<Value, CommandError> {
    control(json!({"command":"orchestration_discard","session_id":session_id,"run_id":run_id,"turn_id":turn_id}), &state).await
}
#[tauri::command]
pub async fn orchestration_trace(
    session_id: String,
    exchange_id: u64,
    state: State<'_, LibraryState>,
) -> Result<Value, CommandError> {
    control(
        json!({"command":"orchestration_trace","session_id":session_id,"exchange_id":exchange_id}),
        &state,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::golden;

    fn request() -> Value {
        json!({"session_id":"session","run_id":"run","turn_id":"turn"})
    }

    #[test]
    fn channel_forwards_golden_and_holds_terminal_until_eof() {
        let events = golden();
        let mut sent = Vec::new();
        forward(
            request(),
            |e| {
                sent.push(e);
                Ok(())
            },
            |_, send| {
                for e in events.clone() {
                    send(e)?;
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(sent, events);
    }

    #[test]
    fn eof_malformed_and_postterminal_each_emit_one_safe_error() {
        let events = golden();
        for cutoff in 0..=events.len() {
            for postterminal in [false, true] {
                let mut sent = Vec::new();
                let result = forward(
                    request(),
                    |e| {
                        sent.push(e);
                        Ok(())
                    },
                    |_, send| {
                        for e in events[..cutoff].iter().cloned() {
                            send(e)?;
                        }
                        if postterminal {
                            send(json!({"raw":"must not reach channel"}))?;
                        }
                        Err(ERROR)
                    },
                );
                assert!(result.is_err());
                assert_eq!(
                    sent.iter()
                        .filter(|e| e["event"] == "done" || e["event"] == "error")
                        .count(),
                    1
                );
                assert_eq!(sent.last().unwrap()["message"], ERROR);
                assert_eq!(sent.last().unwrap()["event"], "error");
                assert!(!sent.iter().any(|e| e.get("raw").is_some()));
            }
        }
    }

    #[test]
    fn channel_disconnect_stops_reader_without_duplicate_delivery() {
        for cutoff in 0..golden().len() - 1 {
            let mut delivered = 0;
            let mut consumed = 0;
            let result = forward(
                request(),
                |_| {
                    delivered += 1;
                    if delivered > cutoff {
                        Err(ERROR)
                    } else {
                        Ok(())
                    }
                },
                |_, send| {
                    for e in golden() {
                        consumed += 1;
                        send(e)?;
                    }
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(consumed, cutoff + 1);
            assert_eq!(delivered, cutoff + 1);
        }
    }
}
