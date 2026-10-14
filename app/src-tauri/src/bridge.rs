//! Subprocess bridge to the Python CLI (`python -m src.cli`).
//!
//! One request is written as JSON to the child's stdin; one JSON response is
//! read from stdout. The child runs from the repository root so the `src`
//! package imports cleanly, and the parent environment (API keys, data-path
//! overrides) is inherited. A non-zero exit or an `ok: false` response is
//! turned into a `CommandError`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};

use serde_json::Value;

use crate::error::CommandError;

const STDERR_CAPTURE_LIMIT: usize = 64 * 1024;

struct StderrCapture {
    bytes: VecDeque<u8>,
    total: usize,
    read_error: Option<String>,
}

impl StderrCapture {
    fn diagnostic(&self) -> String {
        let bytes: Vec<_> = self.bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let diagnostic = text.trim();
        let read_error = self
            .read_error
            .as_deref()
            .map(|error| format!("; stderr read failed: {error}"))
            .unwrap_or_default();
        if self.total > self.bytes.len() {
            format!(
                "[stderr truncated; showing last {} bytes] {diagnostic}{read_error}",
                self.bytes.len()
            )
        } else {
            format!("{diagnostic}{read_error}")
        }
    }
}

fn drain_stderr(mut stderr: ChildStderr) -> StderrCapture {
    let mut capture = StderrCapture {
        bytes: VecDeque::with_capacity(STDERR_CAPTURE_LIMIT),
        total: 0,
        read_error: None,
    };
    let mut chunk = [0_u8; 8192];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                capture.total = capture.total.saturating_add(count);
                let overflow = capture
                    .bytes
                    .len()
                    .saturating_add(count)
                    .saturating_sub(STDERR_CAPTURE_LIMIT);
                capture.bytes.drain(..overflow.min(capture.bytes.len()));
                capture.bytes.extend(&chunk[..count]);
            }
            Err(error) => {
                capture.read_error = Some(error.to_string());
                break;
            }
        }
    }
    capture
}

struct StreamingChild {
    child: Child,
    stderr_drain: Option<JoinHandle<StderrCapture>>,
}

impl StreamingChild {
    fn wait(&mut self) -> Result<ExitStatus, CommandError> {
        self.child
            .wait()
            .map_err(|error| CommandError::Bridge(format!("failed to wait for python: {error}")))
    }

    fn finish_stderr(&mut self) -> Result<StderrCapture, CommandError> {
        self.stderr_drain
            .take()
            .ok_or_else(|| CommandError::Bridge("stderr drain unavailable".to_string()))?
            .join()
            .map_err(|_| CommandError::Bridge("stderr drain thread panicked".to_string()))
    }
}

impl Drop for StreamingChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(drain) = self.stderr_drain.take() {
            let _ = drain.join();
        }
    }
}

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
    on_line: F,
) -> Result<(), CommandError>
where
    F: FnMut(Value),
{
    let executable = python_executable();
    call_streaming_with_executable(project_root, Path::new(&executable), request, on_line)
}

fn call_streaming_with_executable<F>(
    project_root: &Path,
    executable: &Path,
    request: Value,
    mut on_line: F,
) -> Result<(), CommandError>
where
    F: FnMut(Value),
{
    let payload = serde_json::to_vec(&request)
        .map_err(|e| CommandError::Bridge(format!("failed to encode request: {e}")))?;

    let child = Command::new(executable)
        .arg("-m")
        .arg("src.cli")
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CommandError::Bridge(format!("failed to spawn python: {e}")))?;

    let mut process = StreamingChild {
        child,
        stderr_drain: None,
    };
    let stderr = process
        .child
        .stderr
        .take()
        .ok_or_else(|| CommandError::Bridge("child stderr unavailable".to_string()))?;
    process.stderr_drain = Some(
        thread::Builder::new()
            .name("capstone-stderr-drain".to_string())
            .spawn(move || drain_stderr(stderr))
            .map_err(|error| {
                CommandError::Bridge(format!("failed to start stderr drain: {error}"))
            })?,
    );

    let mut stdin = process
        .child
        .stdin
        .take()
        .ok_or_else(|| CommandError::Bridge("child stdin unavailable".to_string()))?;
    stdin
        .write_all(&payload)
        .map_err(|e| CommandError::Bridge(format!("failed to write request: {e}")))?;
    drop(stdin);

    let stdout = process
        .child
        .stdout
        .take()
        .ok_or_else(|| CommandError::Bridge("child stdout unavailable".to_string()))?;

    let mut python_error = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| CommandError::Bridge(format!("failed to read line: {e}")))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| CommandError::InvalidResponse(format!("{e}: {trimmed}")))?;
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            python_error = value.get("error").and_then(Value::as_str).map(str::to_string);
        }
        on_line(value);
    }

    let status = process.wait()?;
    let stderr = process.finish_stderr()?;
    if let (false, Some(message)) = (status.success(), python_error) {
        return Err(CommandError::Bridge(message));
    }
    if !status.success() {
        return Err(CommandError::Bridge(format!(
            "python exited with {status}; stderr: {}",
            stderr.diagnostic()
        )));
    }
    if let Some(error) = stderr.read_error {
        return Err(CommandError::Bridge(format!(
            "failed to read python stderr: {error}"
        )));
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SCRIPT_ID: AtomicU64 = AtomicU64::new(0);

    fn executable_script(contents: &str) -> std::path::PathBuf {
        let suffix = NEXT_SCRIPT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "capstone-stderr-drain-{}-{suffix}.py",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn drains_stderr_beyond_pipe_capacity_while_forwarding_index_events() {
        let script = executable_script(
            "#!/usr/bin/env python3\nimport sys\nsys.stderr.buffer.write(b'x' * 262144)\nsys.stderr.buffer.flush()\nprint('{\"event\":\"start\",\"completed\":0,\"total\":1}', flush=True)\nprint('{\"event\":\"done\",\"completed\":1,\"total\":1,\"indexed\":1,\"empty\":0,\"failed\":0}', flush=True)\n",
        );
        let mut events = Vec::new();

        let result = call_streaming_with_executable(
            Path::new("."),
            &script,
            serde_json::json!({"command": "index_papers", "stream": true}),
            |event| events.push(event),
        );
        let _ = fs::remove_file(script);

        result.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "start");
        assert_eq!(events[1]["event"], "done");
    }

    #[test]
    fn bounds_stderr_diagnostics_on_nonzero_exit() {
        let script = executable_script(
            "#!/usr/bin/env python3\nimport sys\nsys.stderr.buffer.write(b'x' * 262144 + b' useful failure marker\\n')\nsys.stderr.buffer.flush()\nraise SystemExit(17)\n",
        );

        let error =
            call_streaming_with_executable(Path::new("."), &script, serde_json::json!({}), |_| {})
                .expect_err("non-zero child must fail");
        let _ = fs::remove_file(script);
        let message = error.to_string();

        assert!(message.contains("useful failure marker"));
        assert!(message.contains("stderr truncated"));
        assert!(message.len() < STDERR_CAPTURE_LIMIT + 256);
    }

    #[test]
    fn reports_python_error_line_on_nonzero_exit() {
        let script = executable_script(
            "#!/usr/bin/env python3\nprint('{\"ok\": false, \"error\": \"paper indexer unavailable\"}', flush=True)\nraise SystemExit(1)\n",
        );

        let error =
            call_streaming_with_executable(Path::new("."), &script, serde_json::json!({}), |_| {})
                .expect_err("non-zero child must fail");
        let _ = fs::remove_file(script);

        assert_eq!(
            error.to_string(),
            "python bridge failed: paper indexer unavailable"
        );
    }

    #[test]
    fn kills_and_reaps_child_after_invalid_stdout() {
        let pid_path = std::env::temp_dir().join(format!(
            "capstone-stderr-drain-{}-child.pid",
            std::process::id()
        ));
        let script = executable_script(&format!(
            "#!/usr/bin/env python3\nimport os, time\nwith open(r'{}', 'w') as pid_file:\n    pid_file.write(str(os.getpid()))\nprint('invalid json', flush=True)\ntime.sleep(5)\n",
            pid_path.display()
        ));

        let error =
            call_streaming_with_executable(Path::new("."), &script, serde_json::json!({}), |_| {})
                .expect_err("malformed stdout must fail");
        let pid = fs::read_to_string(&pid_path).unwrap();
        let still_running = Command::new("kill")
            .arg("-0")
            .arg(pid.trim())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        let _ = fs::remove_file(script);
        let _ = fs::remove_file(pid_path);

        assert!(matches!(error, CommandError::InvalidResponse(_)));
        assert!(!still_running, "malformed child was not reaped");
    }
}
