use research_backend::{
    protocol::*,
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{RequestContext, serve},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

struct FakeHandler;
impl CommandHandler for FakeHandler {
    fn call(&self, params: Value, ctx: RequestContext) -> BackendFuture<Value> {
        Box::pin(async move {
            match params["mode"].as_str().unwrap_or("stream") {
                "wait" => {
                    ctx.emit("started", json!({})).await?;
                    std::future::pending().await
                }
                "panic" => panic!("synthetic private provider detail"),
                "fail" => Err(ErrorCode::InvalidParams),
                "large_result" => Ok(json!({"text":"x".repeat(MAX_FRAME_BYTES)})),
                "large_event" => {
                    ctx.emit("progress", json!({"text":"x".repeat(MAX_FRAME_BYTES)}))
                        .await?;
                    Ok(json!({}))
                }
                _ => {
                    for index in 0..5 {
                        ctx.emit("progress", json!({"index":index})).await?;
                        tokio::task::yield_now().await;
                    }
                    Ok(json!({"completed":5}))
                }
            }
        })
    }
}

struct Client {
    writer: WriteHalf<DuplexStream>,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Client {
    fn with_registry(registry: Registry) -> Self {
        let (client, server) = tokio::io::duplex(MAX_FRAME_BYTES * 2);
        let (input, output) = tokio::io::split(server);
        let task = tokio::spawn(serve(input, output, registry));
        let (read, writer) = tokio::io::split(client);
        Self {
            writer,
            reader: BufReader::new(read).lines(),
            task,
        }
    }
    fn new() -> Self {
        let mut registry = Registry::foundation();
        registry
            .install("chat_stream", "chat", Arc::new(FakeHandler))
            .unwrap();
        Self::with_registry(registry)
    }
    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
    }
    async fn request(&mut self, id: &str, command: &str, params: Value) {
        self.send(json!({"v":1,"type":"request","id":id,"command":command,"params":params}))
            .await;
    }
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line.len() <= MAX_FRAME_BYTES);
        serde_json::from_str(&line).unwrap()
    }
    async fn close(mut self) -> Vec<Value> {
        self.writer.shutdown().await.unwrap();
        let mut output = Vec::new();
        while let Some(line) = tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
        {
            output.push(serde_json::from_str(&line).unwrap());
        }
        self.task.await.unwrap().unwrap();
        output
    }
}

#[tokio::test]
async fn concurrent_ids_have_independent_order_and_exactly_one_terminal() {
    let mut client = Client::new();
    client.request("first", "chat_stream", json!({})).await;
    client.request("second", "chat_stream", json!({})).await;
    let mut sequences: HashMap<String, Vec<u64>> = HashMap::new();
    let mut terminals = HashMap::new();
    for _ in 0..12 {
        let message = client.next().await;
        let id = message["id"].as_str().unwrap().to_owned();
        sequences
            .entry(id.clone())
            .or_default()
            .push(message["seq"].as_u64().unwrap());
        if message["type"] == "result" {
            *terminals.entry(id).or_insert(0) += 1;
        } else {
            assert_eq!(message["type"], "event");
        }
    }
    for id in ["first", "second"] {
        assert_eq!(sequences[id], [0, 1, 2, 3, 4, 5]);
        assert_eq!(terminals[id], 1);
    }
    assert!(client.close().await.is_empty());
}

#[tokio::test]
async fn cancel_is_terminal_once_unknown_and_late_cancels_are_acknowledged() {
    let mut client = Client::new();
    client
        .request("slow", "chat_stream", json!({"mode":"wait"}))
        .await;
    assert_eq!(client.next().await["type"], "event");
    client.request("health", "health", json!({})).await;
    assert_eq!(client.next().await["type"], "result");
    client
        .send(json!({"v":1,"type":"cancel","id":"slow"}))
        .await;
    assert_eq!(client.next().await["accepted"], true);
    let cancelled = client.next().await;
    assert_eq!(cancelled["type"], "cancelled");
    assert_eq!(cancelled["seq"], 1);
    for id in ["slow", "unknown"] {
        client.send(json!({"v":1,"type":"cancel","id":id})).await;
        let ack = client.next().await;
        assert_eq!(ack["type"], "cancel_ack");
        assert_eq!(ack["accepted"], false);
    }
    assert!(client.close().await.is_empty());
}

#[tokio::test]
async fn duplicate_active_and_completed_ids_do_not_create_second_outcomes() {
    let mut client = Client::new();
    client
        .request("same", "chat_stream", json!({"mode":"wait"}))
        .await;
    client.next().await;
    client.request("same", "health", json!({})).await;
    let duplicate = client.next().await;
    assert_eq!(duplicate["id"], Value::Null);
    assert_eq!(duplicate["error"]["code"], "duplicate_id");
    client.request("done", "health", json!({})).await;
    client.next().await;
    client.request("done", "health", json!({})).await;
    assert_eq!(client.next().await["error"]["code"], "duplicate_id");
    let end = client.close().await;
    assert_eq!(end.len(), 1);
    assert_eq!(end[0]["id"], "same");
    assert_eq!(end[0]["type"], "cancelled");
}

#[tokio::test]
async fn errors_panics_and_oversized_output_do_not_break_the_connection() {
    let mut client = Client::new();
    for (mode, code) in [
        ("fail", "invalid_params"),
        ("panic", "internal"),
        ("large_result", "response_too_large"),
        ("large_event", "response_too_large"),
    ] {
        client
            .request(mode, "chat_stream", json!({"mode":mode}))
            .await;
        let output = client.next().await;
        assert_eq!(output["type"], "error");
        assert_eq!(output["error"]["code"], code);
        assert!(!output.to_string().contains("synthetic private"));
    }
    client.request("still_alive", "health", json!({})).await;
    assert_eq!(client.next().await["data"]["ok"], true);
    assert!(client.close().await.is_empty());
}

#[tokio::test]
async fn malformed_and_oversized_frames_are_drained_and_recovery_is_possible() {
    let mut client = Client::new();
    client.writer.write_all(b"not-json\n\xff\n").await.unwrap();
    for _ in 0..2 {
        assert_eq!(client.next().await["error"]["code"], "invalid_request");
    }
    client
        .writer
        .write_all(&vec![b'x'; MAX_FRAME_BYTES + 8])
        .await
        .unwrap();
    client.writer.write_all(b"\n").await.unwrap();
    assert_eq!(client.next().await["error"]["code"], "frame_too_large");
    client.request("alive", "health", json!({})).await;
    assert_eq!(client.next().await["type"], "result");
    assert!(client.close().await.is_empty());
}

#[tokio::test]
async fn fragmented_input_survives_other_events_and_truncated_eof_is_explicit() {
    let mut client = Client::new();
    client.request("stream", "chat_stream", json!({})).await;
    client
        .writer
        .write_all(b"{\"v\":1,\"type\":\"request\",\"id\":\"frag")
        .await
        .unwrap();
    for _ in 0..6 {
        client.next().await;
    }
    client
        .writer
        .write_all(b"mented\",\"command\":\"health\",\"params\":{}}\n")
        .await
        .unwrap();
    assert_eq!(client.next().await["id"], "fragmented");
    client.writer.write_all(b"{\"unfinished\":").await.unwrap();
    let end = client.close().await;
    assert_eq!(end.len(), 1);
    assert_eq!(end[0]["error"]["code"], "truncated_frame");
}

#[tokio::test]
async fn active_limit_does_not_block_health_or_cancellation() {
    let mut client = Client::new();
    for n in 0..MAX_ACTIVE_REQUESTS {
        client
            .request(&format!("job-{n}"), "chat_stream", json!({"mode":"wait"}))
            .await;
        assert_eq!(client.next().await["type"], "event");
    }
    client.request("busy", "chat_stream", json!({})).await;
    assert_eq!(client.next().await["error"]["code"], "busy");
    client.request("healthy", "health", json!({})).await;
    assert_eq!(client.next().await["type"], "result");
    let end = client.close().await;
    assert_eq!(end.len(), MAX_ACTIVE_REQUESTS);
    assert!(end.iter().all(|o| o["type"] == "cancelled"));
}

#[tokio::test]
async fn request_id_memory_is_bounded_by_connection_limit() {
    let mut client = Client::new();
    for n in 0..MAX_SESSION_REQUESTS {
        client
            .request(&format!("id-{n}"), "not_a_command", json!({}))
            .await;
        assert_eq!(client.next().await["error"]["code"], "unknown_command");
    }
    client.request("over_limit", "health", json!({})).await;
    assert_eq!(client.next().await["error"]["code"], "session_limit");
    client.task.await.unwrap().unwrap();
}

struct BlockingHandler(Arc<AtomicUsize>);
impl CommandHandler for BlockingHandler {
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        let finished = self.0.clone();
        Box::pin(async move {
            context.emit("started", json!({})).await?;
            let token = context.cancellation();
            tokio::task::spawn_blocking(move || {
                while token.check().is_ok() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                finished.fetch_add(1, Ordering::Release);
            })
            .await
            .map_err(|_| ErrorCode::Internal)?;
            Ok(json!({}))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expensive_blocking_work_is_offloaded_and_observes_cancellation() {
    let finished = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::foundation();
    registry
        .install(
            "index_papers_stream",
            "indexing",
            Arc::new(BlockingHandler(finished.clone())),
        )
        .unwrap();
    let mut client = Client::with_registry(registry);
    client
        .request("blocking", "index_papers_stream", json!({}))
        .await;
    client.next().await;
    client.request("responsive", "health", json!({})).await;
    assert_eq!(client.next().await["type"], "result");
    let end = client.close().await;
    assert_eq!(end[0]["type"], "cancelled");
    tokio::time::timeout(Duration::from_secs(3), async {
        while finished.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn registry_enforces_ownership_and_health_reflects_installed_handlers() {
    let mut registry = Registry::foundation();
    assert!(
        registry
            .install("chat_stream", "graph", Arc::new(FakeHandler))
            .is_err()
    );
    assert!(
        registry
            .install("health", "foundation", Arc::new(FakeHandler))
            .is_err()
    );
    registry
        .install("chat_stream", "chat", Arc::new(FakeHandler))
        .unwrap();
    assert!(
        registry
            .install("chat_stream", "chat", Arc::new(FakeHandler))
            .is_err()
    );
    assert!(
        registry.health()["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["command"] == "chat_stream" && c["available"] == true)
    );
}

#[tokio::test]
async fn completion_cancel_races_never_double_terminate_or_emit_after_terminal() {
    let mut client = Client::new();
    for n in 0..50 {
        let id = format!("race-{n}");
        client.request(&id, "chat_stream", json!({})).await;
        client.send(json!({"v":1,"type":"cancel","id":id})).await;
        let mut terminal_count = 0;
        let mut expected_seq = 0;
        let mut acknowledged = false;
        while !acknowledged || terminal_count == 0 {
            let output = client.next().await;
            assert_eq!(output["id"], id);
            if output["type"] == "cancel_ack" {
                assert!(!acknowledged);
                acknowledged = true;
                continue;
            }
            assert_eq!(terminal_count, 0, "nothing may follow a terminal frame");
            assert_eq!(output["seq"], expected_seq);
            expected_seq += 1;
            if output["type"] != "event" {
                terminal_count += 1;
            }
        }
        assert_eq!(terminal_count, 1);
        client
            .request(&format!("fence-{n}"), "health", json!({}))
            .await;
        assert_eq!(client.next().await["id"], format!("fence-{n}"));
    }
    assert!(client.close().await.is_empty());
}

#[tokio::test]
async fn exact_input_frame_limit_is_accepted() {
    let mut client = Client::new();
    let mut frame = serde_json::to_vec(
        &json!({"v":1,"type":"request","id":"exact","command":"health","params":{}}),
    )
    .unwrap();
    frame.resize(MAX_FRAME_BYTES, b' ');
    frame.push(b'\n');
    client.writer.write_all(&frame).await.unwrap();
    assert_eq!(client.next().await["type"], "result");
    assert!(client.close().await.is_empty());
}

#[tokio::test]
async fn non_stream_commands_reuse_work_without_forwarding_progress() {
    let mut registry = Registry::foundation();
    registry
        .install("chat", "chat", Arc::new(FakeHandler))
        .unwrap();
    let mut client = Client::with_registry(registry);
    client.request("sync", "chat", json!({})).await;
    let output = client.next().await;
    assert_eq!(output["type"], "result");
    assert_eq!(output["seq"], 0);
    assert_eq!(output["data"]["completed"], 5);
    assert!(client.close().await.is_empty());
}
