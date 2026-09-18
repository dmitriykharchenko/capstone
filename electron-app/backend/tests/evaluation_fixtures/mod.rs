#![allow(dead_code)]
// Native-only fixtures ported from test_evaluation_runner.py/test_evaluation_results.py.
// Reuse the approved adaptive RAG fake pipeline to test actual core parity.
#[path = "../rag_fixtures/mod.rs"]
pub mod rag;
use research_backend::{
    config::StoragePaths,
    domain::{EvaluationCase, Message},
    evaluations::{
        EvaluationArtifact, EvaluationRequest, JUDGE_CRITERIA, NativeEvaluationService,
        RESULTS_FILENAME,
    },
    rag::{ModelFailure, ModelFuture, RagModel},
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{Cancellation, RequestContext, serve},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf},
    task::JoinHandle,
};

pub fn scores(score: u8) -> Value {
    let mut scores = serde_json::Map::new();
    for name in JUDGE_CRITERIA {
        scores.insert(name.into(), json!({"score":score,"confidence":"high","reasoning":"The answer cites the retrieved evidence.","flags":[]}));
    }
    for (name, key, value) in [
        (
            "relevant_chunk_ranking",
            "relevant_chunk_ranks",
            json!([1, 35]),
        ),
        (
            "context_utilization",
            "chunks_used",
            json!([1, "chunk two"]),
        ),
        ("semantic_similarity", "contradicting_claims", json!([])),
        ("completeness", "missing_elements", json!([])),
        ("hallucination_rate", "unsupported_claims", json!([])),
    ] {
        scores.get_mut(name).unwrap()[key] = value;
    }
    Value::Object(scores)
}
#[derive(Default)]
pub struct Judge {
    pub messages: Mutex<Vec<Vec<Message>>>,
    pub count: AtomicUsize,
    pub behavior: Mutex<HashMap<usize, rag::Behavior>>,
}
impl RagModel for Judge {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation) -> ModelFuture {
        let index = self.count.fetch_add(1, Ordering::SeqCst);
        self.messages.lock().unwrap().push(messages);
        let behavior = self.behavior.lock().unwrap().get(&index).cloned();
        Box::pin(async move {
            match behavior {
                Some(rag::Behavior::Fail(error)) => Err(ModelFailure::Provider(error)),
                Some(rag::Behavior::Text(text)) => Ok(text),
                Some(rag::Behavior::Cancel) => {
                    cancellation.cancel();
                    Ok(scores(4).to_string())
                }
                Some(rag::Behavior::Pending(ready, dropped)) => {
                    let _guard = rag::DropFlag(dropped);
                    ready.notify_one();
                    std::future::pending().await
                }
                None => Ok(scores(4).to_string()),
            }
        })
    }
    fn stream_final(&self, _: Vec<Message>, _: RequestContext) -> ModelFuture {
        panic!("judge must be private")
    }
}
pub fn case(question: &str) -> EvaluationCase {
    EvaluationCase {
        question: question.into(),
        grading_notes: "Grounded reference — café".into(),
        sources: vec!["alpha.pdf".into(), "beta.pdf".into()],
    }
}
pub fn paths(root: &Path) -> StoragePaths {
    let root = root.canonicalize().unwrap();
    StoragePaths {
        data_root: root.join("data"),
        evaluation_results: root.join("data/evaluations"),
        history_database: root.join("data/history.sqlite"),
        vector_database: root.join("data/vector.sqlite"),
        graph_database: root.join("data/graph.sqlite"),
        bm25_index: root.join("data/bm25.json"),
        resources: root.join("resources"),
        papers: root.join("papers"),
    }
}
pub fn artifact(paths: &StoragePaths) -> EvaluationArtifact {
    EvaluationArtifact::parse(
        &std::fs::read_to_string(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap(),
    )
    .unwrap()
}
pub struct Handler(pub NativeEvaluationService);
impl CommandHandler for Handler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let service = self.0.clone();
        Box::pin(async move {
            service
                .run_request(EvaluationRequest::from_value(params)?, context)
                .await
        })
    }
}
pub struct Runtime {
    pub input: WriteHalf<DuplexStream>,
    pub output: Lines<BufReader<ReadHalf<DuplexStream>>>,
    pub task: JoinHandle<std::io::Result<()>>,
}
impl Runtime {
    pub fn new(service: NativeEvaluationService) -> Self {
        Self::with_capacity(service, 4 * 1024 * 1024)
    }
    pub fn with_capacity(service: NativeEvaluationService, capacity: usize) -> Self {
        let mut registry = Registry::foundation();
        registry
            .install("evaluate_stream", "evaluations", Arc::new(Handler(service)))
            .unwrap();
        let (client, server) = tokio::io::duplex(capacity);
        let (read, input) = tokio::io::split(client);
        let (reader, writer) = tokio::io::split(server);
        let task = tokio::spawn(serve(reader, writer, registry));
        Self {
            input,
            output: BufReader::new(read).lines(),
            task,
        }
    }
    pub async fn send(&mut self, value: Value) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    pub async fn request(&mut self, id: &str, cases: Vec<EvaluationCase>) {
        self.send(json!({"v":1,"id":id,"type":"request","command":"evaluate_stream","params":{"retrieval_type":"hybrid","test_cases":cases}})).await;
    }
    pub async fn ready(&mut self, ready: &tokio::sync::Notify) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    _ = ready.notified() => return,
                    frame = self.next() => assert_eq!(frame["type"], "event", "early terminal: {frame}"),
                }
            }
        }).await.expect("stage did not start");
    }
    pub async fn next(&mut self) -> Value {
        serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(10), self.output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }
    pub async fn terminal(&mut self, id: &str) -> Vec<Value> {
        let mut frames = vec![];
        loop {
            let frame = self.next().await;
            let terminal = frame["id"] == id
                && matches!(
                    frame["type"].as_str(),
                    Some("result" | "error" | "cancelled")
                );
            frames.push(frame);
            if terminal {
                return frames;
            }
        }
    }
    pub async fn finish(mut self) {
        self.input.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(self.output.next_line().await.unwrap().is_none());
    }
}

pub async fn seed(paths: &StoragePaths) -> Vec<u8> {
    let (_, _, pipeline, _) = rag::fixtures(&[]);
    let mut runtime = Runtime::new(
        NativeEvaluationService::new(pipeline, Arc::new(Judge::default()), paths).unwrap(),
    );
    runtime
        .request("seed", vec![case("prior valid result")])
        .await;
    assert_eq!(
        runtime.terminal("seed").await.last().unwrap()["type"],
        "result"
    );
    runtime.finish().await;
    std::fs::read(paths.evaluation_results.join(RESULTS_FILENAME)).unwrap()
}

pub fn node_validate(frames: &[Value]) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new("node").arg("--input-type=module").arg("-e").arg(r#"
      import { parseEvent, terminalEvent } from './electron/adapters.mjs';
      import assert from 'node:assert/strict';
      let input=''; for await (const chunk of process.stdin) input+=chunk;
      for (const frame of JSON.parse(input)) {
        if(frame.type==='event') { const ui=parseEvent('evaluate_stream',frame.event,frame.data); assert(!('case_index' in ui)); }
        else if(frame.type==='result') assert.equal(terminalEvent('evaluate_stream',frame.data).event,'complete');
      }
    "#).current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(frames).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Node validators: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
