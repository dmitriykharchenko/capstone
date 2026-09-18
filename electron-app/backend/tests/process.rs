use research_backend::protocol::MAX_FRAME_BYTES;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    output: mpsc::Receiver<Value>,
    _root: tempfile::TempDir,
}
impl Process {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("research-backend");
        std::fs::copy(env!("CARGO_BIN_EXE_research-backend"), &binary).unwrap();
        // No Python executable, credentials, repository cwd or legacy resources.
        let mut child = Command::new(binary)
            .current_dir(root.path())
            .env_clear()
            .env("PATH", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                assert!(line.len() <= MAX_FRAME_BYTES);
                let parsed =
                    serde_json::from_str(&line).expect("stdout must contain only protocol JSON");
                if sender.send(parsed).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            output,
            _root: root,
        }
    }
    fn send(&mut self, value: Value) {
        let input = self.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &value).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }
    fn next(&self) -> Value {
        self.output.recv_timeout(Duration::from_secs(3)).unwrap()
    }
    fn finish(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "backend must exit promptly on stdin EOF"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(stderr.is_empty(), "normal operation emits no diagnostics");
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn copied_executable_is_long_lived_without_python_or_repository_resources() {
    let mut process = Process::start();
    let pid = process.child.id();
    for id in ["first", "second"] {
        process.send(json!({"v":1,"type":"request","id":id,"command":"health","params":{}}));
        let health = process.next();
        assert_eq!(health["id"], id);
        assert_eq!(health["type"], "result");
        assert_eq!(health["data"]["backend"], "rust");
        assert_eq!(health["seq"], 0);
        assert_eq!(process.child.id(), pid);
        assert!(process.child.try_wait().unwrap().is_none());
    }
    process.finish();
    assert!(process.output.try_recv().is_err());
}

#[test]
fn every_domain_command_explicitly_reports_unavailable_in_the_real_binary() {
    let mut process = Process::start();
    let requests: Value =
        serde_json::from_str(include_str!("../../contracts/command-requests.json")).unwrap();
    for request in requests.as_array().unwrap() {
        if request["command"] == "health" {
            continue;
        }
        let command = request["command"].as_str().unwrap();
        process.send(request.clone());
        let output = process.next();
        assert_eq!(output["id"], command);
        assert_eq!(output["type"], "error");
        assert_eq!(output["error"]["code"], "unavailable");
    }
    process.finish();
}

#[test]
fn actual_stdio_recovers_after_invalid_input_and_handles_cancellation_control() {
    let mut process = Process::start();
    process
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"not-json\n")
        .unwrap();
    assert_eq!(process.next()["error"]["code"], "invalid_request");
    process.send(json!({"v":99,"type":"request","id":"bad","command":"health","params":{}}));
    assert_eq!(process.next()["error"]["code"], "unsupported_version");
    process.send(json!({"v":1,"type":"cancel","id":"unknown"}));
    assert_eq!(process.next()["accepted"], false);
    process.send(json!({"v":1,"type":"request","id":"alive","command":"health","params":{}}));
    assert_eq!(process.next()["data"]["ok"], true);
    process.finish();
}

#[test]
fn broken_stdout_exits_even_if_stdin_is_still_open() {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_research-backend"))
        .current_dir(root.path())
        .env_clear()
        .env("PATH", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            b"{\"v\":1,\"type\":\"request\",\"id\":\"h\",\"command\":\"health\",\"params\":{}}\n",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("backend hung on broken output");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(!status.success());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(stderr.trim(), "backend: transport closed");
}
