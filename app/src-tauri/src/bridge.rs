//! Subprocess bridge to the Python CLI (`python -m src.cli`).
//!
//! One request is written as JSON to the child's stdin; one JSON response is
//! read from stdout. The child runs from the repository root so the `src`
//! package imports cleanly, and the parent environment (API keys, data-path
//! overrides) is inherited. A non-zero exit or an `ok: false` response is
//! turned into a `CommandError`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::error::CommandError;

/// Return the Python interpreter to invoke, honoring `CAPSTONE_PYTHON`.
///
/// An unset or empty/whitespace-only `CAPSTONE_PYTHON` falls back to `python3`,
/// so a shell substitution that resolves to an empty string (e.g. conda not
/// found) never spawns an empty command.
fn python_executable() -> String {
    match std::env::var("CAPSTONE_PYTHON") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "python3".to_string(),
    }
}

/// Send `request` to the Python CLI and return the parsed JSON response.
pub fn call(project_root: &Path, request: Value) -> Result<Value, CommandError> {
    let mut child = Command::new(python_executable())
        .arg("-m")
        .arg("src.cli")
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CommandError::Bridge(format!("failed to spawn python: {e}")))?;

    let payload = serde_json::to_vec(&request)
        .map_err(|e| CommandError::Bridge(format!("failed to encode request: {e}")))?;

    child
        .stdin
        .take()
        .ok_or_else(|| CommandError::Bridge("child stdin unavailable".to_string()))?
        .write_all(&payload)
        .map_err(|e| CommandError::Bridge(format!("failed to write request: {e}")))?;

    let output = child
        .wait_with_output()
        .map_err(|e| CommandError::Bridge(format!("failed to read output: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        CommandError::InvalidResponse(format!("{e}; stderr: {}", stderr.trim()))
    })?;

    if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        return Err(CommandError::Bridge(message));
    }

    Ok(parsed)
}

/// Send `request` to the Python CLI and invoke `on_line` for each JSON line the
/// child writes to stdout, forwarding streamed events as they are produced.
///
/// The child emits newline-delimited JSON (one delta per line, then a final
/// object). `on_line` is called for every parsed line; a non-zero exit with no
/// parsed error line is surfaced as a `CommandError` including stderr.
pub fn call_streaming<F>(
    project_root: &Path,
    request: Value,
    mut on_line: F,
) -> Result<(), CommandError>
where
    F: FnMut(Value),
{
    let mut child = Command::new(python_executable())
        .arg("-m")
        .arg("src.cli")
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CommandError::Bridge(format!("failed to spawn python: {e}")))?;

    let payload = serde_json::to_vec(&request)
        .map_err(|e| CommandError::Bridge(format!("failed to encode request: {e}")))?;

    child
        .stdin
        .take()
        .ok_or_else(|| CommandError::Bridge("child stdin unavailable".to_string()))?
        .write_all(&payload)
        .map_err(|e| CommandError::Bridge(format!("failed to write request: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CommandError::Bridge("child stdout unavailable".to_string()))?;

    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| CommandError::Bridge(format!("failed to read line: {e}")))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| CommandError::InvalidResponse(format!("{e}: {trimmed}")))?;
        on_line(value);
    }

    let status = child
        .wait()
        .map_err(|e| CommandError::Bridge(format!("failed to wait for python: {e}")))?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut err) = child.stderr.take() {
            use std::io::Read;
            let _ = err.read_to_string(&mut stderr);
        }
        return Err(CommandError::Bridge(format!(
            "python exited with {status}; stderr: {}",
            stderr.trim()
        )));
    }

    Ok(())
}
