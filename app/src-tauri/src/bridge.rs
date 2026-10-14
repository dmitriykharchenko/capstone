//! Subprocess bridge to the development Python CLI or bundled frozen backend.
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

use crate::{error::CommandError, BackendInvocation};

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

fn command_for_backend(backend: &BackendInvocation) -> Command {
    let mut command = Command::new(&backend.executable);
    if !backend.frozen {
        command.args(["-m", "src.cli"]);
    }
    command
        .current_dir(&backend.working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if backend.frozen
        && std::env::var_os("CAPSTONE_DATA_DIR")
            .map_or(true, |value| value.to_string_lossy().trim().is_empty())
    {
        if let Some(data_directory) = &backend.data_directory {
            command.env("CAPSTONE_DATA_DIR", data_directory);
        }
    }
    if let Some(library_directory) = &backend.library_directory {
        command.env("CAPSTONE_LIBRARY_DIR", library_directory);
    }
    command
}

/// Send `request` to the Python CLI and return the parsed JSON response.
pub fn call(
    project_root: &Path,
    library_directory: Option<&Path>,
    request: Value,
) -> Result<Value, CommandError> {
    let backend = crate::backend_invocation(project_root, library_directory);
    call_with_backend(&backend, request)
}

fn call_with_backend(backend: &BackendInvocation, request: Value) -> Result<Value, CommandError> {
    let mut child = command_for_backend(&backend)
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
    library_directory: Option<&Path>,
    request: Value,
    on_line: F,
) -> Result<(), CommandError>
where
    F: FnMut(Value),
{
    let backend = crate::backend_invocation(project_root, library_directory);
    call_streaming_with_backend(&backend, request, on_line)
}

#[cfg(test)]
fn call_streaming_with_executable<F>(
    project_root: &Path,
    executable: &Path,
    request: Value,
    on_line: F,
) -> Result<(), CommandError>
where
    F: FnMut(Value),
{
    let backend = BackendInvocation {
        executable: executable.to_path_buf(),
        working_directory: project_root.to_path_buf(),
        frozen: false,
        data_directory: None,
        library_directory: None,
    };
    call_streaming_with_backend(&backend, request, on_line)
}

fn call_streaming_with_backend<F>(
    backend: &BackendInvocation,
    request: Value,
    mut on_line: F,
) -> Result<(), CommandError>
where
    F: FnMut(Value),
{
    let payload = serde_json::to_vec(&request)
        .map_err(|e| CommandError::Bridge(format!("failed to encode request: {e}")))?;

    let child = command_for_backend(backend)
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
            python_error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string);
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
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
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

    #[test]
    fn frozen_backend_runs_its_resource_executable_with_writable_data_path() {
        let inherited_library = std::env::var_os("CAPSTONE_LIBRARY_DIR");
        let root = std::env::temp_dir().join(format!(
            "capstone-frozen-bridge-{}",
            NEXT_SCRIPT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let executable = root.join("resources/backend/macos-arm64/capstone-cli/capstone-cli");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::create_dir_all(executable.parent().unwrap().join("_internal")).unwrap();
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '{\"ok\":true,\"cwd\":\"%s\",\"data\":\"%s\",\"library\":\"%s\"}\\n' \"$PWD\" \"$CAPSTONE_DATA_DIR\" \"$CAPSTONE_LIBRARY_DIR\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let data_directory = root.join("Application Support/Capstone");
        let backend = BackendInvocation {
            executable: executable.clone(),
            working_directory: executable.parent().unwrap().to_path_buf(),
            frozen: true,
            data_directory: Some(data_directory.clone()),
            library_directory: Some(root.join("Papers — 研究")),
        };

        let result = call_streaming_with_backend(&backend, serde_json::json!({}), |_| {});
        let output = call_with_backend(&backend, serde_json::json!({})).unwrap();

        assert!(result.is_ok());
        assert!(executable.parent().unwrap().join("_internal").is_dir());
        assert_eq!(
            output["cwd"].as_str(),
            fs::canonicalize(executable.parent().unwrap())
                .unwrap()
                .to_str()
        );
        assert_eq!(output["data"].as_str(), data_directory.to_str());
        assert_eq!(
            output["library"].as_str(),
            backend.library_directory.as_deref().and_then(Path::to_str)
        );
        assert_eq!(std::env::var_os("CAPSTONE_LIBRARY_DIR"), inherited_library);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_only_overrides_library_for_selected_invocation() {
        let mut backend = BackendInvocation::development(Path::new("/source"));
        let unselected = command_for_backend(&backend);
        assert!(!unselected
            .get_envs()
            .any(|(key, _)| key == "CAPSTONE_LIBRARY_DIR"));

        backend.library_directory = Some(PathBuf::from("/Papers with spaces/研究"));
        let selected = command_for_backend(&backend);
        assert!(selected.get_envs().any(|(key, value)| {
            key == "CAPSTONE_LIBRARY_DIR" && value == Some(OsStr::new("/Papers with spaces/研究"))
        }));
    }
}
