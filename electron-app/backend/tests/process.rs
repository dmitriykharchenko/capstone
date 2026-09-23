use research_backend::protocol::MAX_FRAME_BYTES;
#[allow(dead_code)]
mod papers_fixtures;
use papers_fixtures::{Page, pdf};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};

// Fresh executable copies compete for startup resources. Serialize only their
// copy/spawn/health handshake; each test's later protocol work remains parallel.
static STARTUP_LOCK: Mutex<()> = Mutex::new(());

struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    output: mpsc::Receiver<Value>,
    _root: tempfile::TempDir,
}
impl Process {
    fn start() -> Self {
        Self::configured(true)
    }
    fn configured(configured: bool) -> Self {
        let startup = STARTUP_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("research-backend");
        std::fs::copy(env!("CARGO_BIN_EXE_research-backend"), &binary).unwrap();
        // No Python executable, credentials, repository cwd or legacy resources.
        let resources = root.path().join("resources");
        let papers = resources.join("ResearchPapers");
        std::fs::create_dir_all(&papers).unwrap();
        std::fs::write(
            papers.join("Test.pdf"),
            pdf(vec![Page::Text(vec![b"Native preview".to_vec()])]),
        )
        .unwrap();
        let mut command = Command::new(binary);
        command
            .current_dir(root.path())
            .env_clear()
            .env("PATH", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if configured {
            command
                .env("CAPSTONE_APP_DATA_ROOT", root.path().join("data"))
                .env("CAPSTONE_RESOURCES_DIR", &resources)
                .env("CAPSTONE_PAPERS_DIR", &papers);
        }
        let mut child = command.spawn().unwrap();
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
        let mut process = Self {
            child,
            stdin,
            output,
            _root: root,
        };
        process.send(json!({"v":1,"type":"request","id":"startup","command":"health","params":{}}));
        let health = process.next();
        assert_eq!(health["v"], 1);
        assert_eq!(health["type"], "result");
        assert_eq!(health["id"], "startup");
        assert_eq!(health["seq"], 0);
        assert_eq!(health["data"]["backend"], "rust");
        assert_eq!(health["data"]["ok"], true);
        assert!(process.child.try_wait().unwrap().is_none());
        drop(startup);
        process
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
fn unfinished_domain_commands_report_unavailable_in_the_configured_binary() {
    let mut process = Process::start();
    let requests: Value =
        serde_json::from_str(include_str!("../../contracts/command-requests.json")).unwrap();
    for request in requests.as_array().unwrap() {
        if [
            "health",
            "list_papers",
            "get_paper",
            "read_pdf",
            "read_pdf_range",
        ]
        .contains(&request["command"].as_str().unwrap())
        {
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
fn unconfigured_binary_is_health_only_without_cwd_discovery() {
    let mut process = Process::configured(false);
    process.send(json!({"v":1,"type":"request","id":"health","command":"health","params":{}}));
    let health = process.next();
    let commands = health["data"]["commands"].as_array().unwrap();
    assert_eq!(
        commands.iter().filter(|c| c["available"] == true).count(),
        1
    );
    process.send(json!({"v":1,"type":"request","id":"list","command":"list_papers","params":{}}));
    assert_eq!(process.next()["error"]["code"], "unavailable");
    process.finish();
}

#[test]
fn real_binary_lists_gets_and_streams_only_configured_pdf_bytes() {
    let mut process = Process::start();
    process.send(json!({"v":1,"type":"request","id":"health","command":"health","params":{}}));
    let health = process.next();
    assert_eq!(health["data"]["stage"], "paper_browsing");
    assert_eq!(
        health["data"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["available"] == true)
            .count(),
        5
    );
    for (id, command, params) in [
        ("list", "list_papers", json!({})),
        ("get", "get_paper", json!({"paper_id":"Test"})),
    ] {
        process.send(json!({"v":1,"type":"request","id":id,"command":command,"params":params}));
        let result = process.next();
        assert_eq!(result["type"], "result");
        let paper = if command == "list_papers" {
            &result["data"][0]
        } else {
            &result["data"]
        };
        assert_eq!(paper["id"], "Test");
        assert!(!paper.to_string().contains("resources"));
    }
    process.send(json!({"v":1,"type":"request","id":"read","command":"read_pdf","params":{"paper_id":"Test"}}));
    let mut bytes = Vec::new();
    let meta = process.next();
    assert_eq!(meta["event"], "pdf_open");
    let length = meta["data"]["total_bytes"].as_u64().unwrap();
    for begin in (0..length).step_by(64 * 1024) {
        process.send(json!({"v":1,"type":"request","id":format!("range-{begin}"),"command":"read_pdf_range","params":{"preview_id":meta["data"]["preview_id"],"begin":begin,"end":length.min(begin + 64 * 1024)}}));
        let result = process.next();
        assert_eq!(result["type"], "result");
        assert_eq!(result["data"]["offset"], bytes.len());
        let chunk: Vec<u8> = serde_json::from_value(result["data"]["bytes"].clone()).unwrap();
        assert!(chunk.len() <= 64 * 1024);
        bytes.extend(chunk);
    }
    assert_eq!(
        bytes,
        std::fs::read(
            process
                ._root
                .path()
                .join("resources/ResearchPapers/Test.pdf")
        )
        .unwrap()
    );
    let encrypted = papers_fixtures::encrypted_pdf(true);
    std::fs::write(
        process
            ._root
            .path()
            .join("resources/ResearchPapers/Locked.pdf"),
        &encrypted,
    )
    .unwrap();
    process.send(json!({"v":1,"type":"request","id":"locked","command":"read_pdf","params":{"paper_id":"Locked"}}));
    let mut transferred = Vec::new();
    let meta = process.next();
    assert_eq!(meta["event"], "pdf_open");
    for begin in (0..encrypted.len()).step_by(64 * 1024) {
        process.send(json!({"v":1,"type":"request","id":format!("locked-range-{begin}"),"command":"read_pdf_range","params":{"preview_id":meta["data"]["preview_id"],"begin":begin,"end":encrypted.len().min(begin + 64 * 1024)}}));
        let output = process.next();
        assert_eq!(output["type"], "result");
        assert_eq!(output["data"]["offset"], transferred.len());
        let chunk: Vec<u8> = serde_json::from_value(output["data"]["bytes"].clone()).unwrap();
        transferred.extend(chunk);
    }
    assert!(
        transferred == encrypted,
        "encrypted preview bytes must remain unchanged"
    );
    for (id, paper) in [
        ("missing", "missing"),
        ("traversal", "../Test"),
        ("absolute", "/Test"),
    ] {
        process.send(json!({"v":1,"type":"request","id":id,"command":"read_pdf","params":{"paper_id":paper}}));
        assert_eq!(
            process.next()["error"]["code"],
            if paper == "missing" {
                "pdf_invalid"
            } else {
                "invalid_params"
            }
        );
    }
    assert!(!process._root.path().join("data").exists());
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
