#![allow(dead_code)]

use std::{sync::Arc, time::Duration};

use research_backend::{
    domain::Message,
    registry::{CommandHandler, Registry},
    runtime::serve,
};
use serde_json::{Value, json};
use tokio::{
    io::{
        AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf,
        WriteHalf,
    },
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};

pub fn messages(text: &str) -> Vec<Message> {
    vec![Message {
        role: "user".into(),
        content: text.into(),
        exchange_id: Some(42),
        sources: vec![],
    }]
}

pub fn completion(text: &str) -> Value {
    json!({"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
}

pub fn delta(content: Value, reason: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices":[{"index":0,"delta":content,"finish_reason":reason}]})
    )
}

pub struct Plan {
    pub status: u16,
    pub content_type: &'static str,
    pub chunks: Vec<Vec<u8>>,
    pub delay: Duration,
    pub hang: bool,
    pub advertised_length: Option<usize>,
}

impl Plan {
    pub fn json(value: Value) -> Self {
        Self::raw(value.to_string().into_bytes(), "application/json")
    }
    pub fn raw(body: Vec<u8>, content_type: &'static str) -> Self {
        Self {
            status: 200,
            content_type,
            chunks: vec![body],
            delay: Duration::ZERO,
            hang: false,
            advertised_length: None,
        }
    }
    pub fn sse(body: String, chunk_size: usize) -> Self {
        let mut plan = Self::raw(vec![], "text/event-stream; charset=utf-8");
        plan.chunks = body
            .as_bytes()
            .chunks(chunk_size)
            .map(<[u8]>::to_vec)
            .collect();
        plan
    }
}

#[derive(Debug)]
pub struct Request {
    pub path: String,
    pub body: Value,
    pub has_authorization: bool,
}

/// Every provider test binds its own loopback listener. No configured environment,
/// credentials, user databases or remote endpoints are ever used.
pub struct FakeHttp {
    pub url: String,
    requests: mpsc::UnboundedReceiver<Request>,
    closed: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl FakeHttp {
    pub async fn new(plans: Vec<Plan>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/compatible/v1", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::unbounded_channel();
        let (closed_tx, closed) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for plan in plans {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = 0;
                let mut has_authorization = false;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                    if name.eq_ignore_ascii_case("authorization") {
                        has_authorization = true;
                    }
                }
                assert!(length <= 8 * 1024 * 1024);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                tx.send(Request {
                    path,
                    body: serde_json::from_slice(&body).unwrap(),
                    has_authorization,
                })
                .unwrap();
                tokio::time::sleep(plan.delay).await;
                let mut socket = reader.into_inner();
                let framing = plan
                    .advertised_length
                    .map(|n| format!("Content-Length: {n}"))
                    .unwrap_or_else(|| "Transfer-Encoding: chunked".into());
                let headers = format!(
                    "HTTP/1.1 {} Test\r\nContent-Type: {}\r\n{}\r\nConnection: close\r\n\r\n",
                    plan.status, plan.content_type, framing
                );
                if socket.write_all(headers.as_bytes()).await.is_err() {
                    continue;
                }
                let mut broken = false;
                for chunk in plan.chunks {
                    let bytes = if plan.advertised_length.is_some() {
                        chunk
                    } else {
                        let mut bytes = format!("{:x}\r\n", chunk.len()).into_bytes();
                        bytes.extend(chunk);
                        bytes.extend_from_slice(b"\r\n");
                        bytes
                    };
                    if socket.write_all(&bytes).await.is_err() {
                        broken = true;
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                if plan.hang && !broken {
                    let mut probe = [0];
                    if matches!(socket.read(&mut probe).await, Ok(0) | Err(_)) {
                        let _ = closed_tx.send(());
                    }
                } else if !broken && plan.advertised_length.is_none() {
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                }
            }
        });
        Self {
            url,
            requests,
            closed,
            task,
        }
    }

    pub async fn request(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }

    pub async fn wait_closed(&mut self) {
        tokio::time::timeout(Duration::from_secs(3), self.closed.recv())
            .await
            .unwrap()
            .unwrap();
    }

    pub fn remaining_requests(&mut self) -> usize {
        let mut count = 0;
        while self.requests.try_recv().is_ok() {
            count += 1;
        }
        count
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct RuntimeClient {
    writer: WriteHalf<DuplexStream>,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl RuntimeClient {
    pub fn new(handler: Arc<dyn CommandHandler>) -> Self {
        let mut registry = Registry::foundation();
        registry
            .install("chat_stream", "chat", handler.clone())
            .unwrap();
        registry.install("chat", "chat", handler).unwrap();
        let (client, server) = tokio::io::duplex(2 * 1024 * 1024);
        let (input, output) = tokio::io::split(server);
        let task = tokio::spawn(serve(input, output, registry));
        let (read, writer) = tokio::io::split(client);
        Self {
            writer,
            reader: BufReader::new(read).lines(),
            task,
        }
    }
    pub async fn send(&mut self, value: Value) {
        self.writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    pub async fn request(&mut self, command: &str, params: Value) {
        self.send(json!({"v":1,"type":"request","id":"p","command":command,"params":params}))
            .await;
    }
    pub async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    pub async fn finish(mut self) -> Vec<Value> {
        self.writer.shutdown().await.unwrap();
        let mut output = vec![];
        while let Some(line) = tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
        {
            output.push(serde_json::from_str(&line).unwrap());
        }
        (&mut self.task).await.unwrap().unwrap();
        output
    }
}

impl Drop for RuntimeClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}
