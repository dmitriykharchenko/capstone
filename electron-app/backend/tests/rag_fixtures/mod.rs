#![allow(dead_code)]
// Native fixtures ported from tests/test_agent_loop_multihop.py StageModel,
// HopRetriever/results(), test_agent_loop.py provenance/truncation fixtures and
// test_evaluation_runner.py Chat/Evals parity. No Python or legacy runtime reads.
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use research_backend::{
    domain::{Chunk, Message, RetrievalScope, RetrievalStrategy, SourceRecord},
    protocol::ErrorCode,
    providers::{ModelClient, ProviderError},
    rag::{
        AdaptiveRag, ModelFailure, ModelFuture, RagError, RagModel, RagPipeline, RagRun, RunMode,
    },
    registry::{BackendFuture, CommandHandler, CommandSpec, Registry},
    retrieval::Retriever,
    runtime::{Cancellation, RequestContext, serve},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf},
    sync::{Notify, mpsc},
    task::JoinHandle,
};

pub const SYSTEM: &str = "Be precise. PRIVATE_SYSTEM_MARKER";
pub const QUESTION: &str = "compare alpha and beta";

#[derive(Clone, Debug)]
pub enum Call {
    Model {
        stage: &'static str,
        messages: Vec<Message>,
        streaming: bool,
    },
    Retrieve {
        query: String,
        strategy: RetrievalStrategy,
        scope: RetrievalScope,
        limit: usize,
    },
}

pub type Calls = Arc<Mutex<Vec<Call>>>;

#[derive(Clone)]
pub enum Behavior {
    Fail(ProviderError),
    Text(String),
    Cancel,
    Pending(Arc<Notify>, Arc<AtomicBool>),
}

pub struct DropFlag(pub Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub struct FakeModel {
    pub calls: Calls,
    pub plan: String,
    pub coverage: String,
    pub behavior: Mutex<HashMap<usize, Behavior>>,
    pub count: AtomicUsize,
    pub final_parts: Vec<String>,
    pub stream_error: Option<ProviderError>,
    pub stream_pending: Option<(Arc<Notify>, Arc<AtomicBool>)>,
}

impl FakeModel {
    pub fn new(calls: Calls, initial: &[&str], followups: &[&str]) -> Self {
        Self {
            calls,
            plan: json!({"subqueries":initial}).to_string(),
            coverage: json!({"complete":followups.is_empty(),"follow_up_subqueries":followups})
                .to_string(),
            behavior: Mutex::default(),
            count: AtomicUsize::new(0),
            final_parts: vec!["final ".into(), "answer".into()],
            stream_error: None,
            stream_pending: None,
        }
    }

    fn response(
        &self,
        messages: Vec<Message>,
        streaming: bool,
        cancellation: Cancellation,
    ) -> ModelFuture {
        let stage = stage(&messages);
        let response = match stage {
            "planner" => self.plan.clone(),
            "coverage" => self.coverage.clone(),
            "subanswer" => format!(
                "summary for {}",
                payload(&messages)["subquery"].as_str().unwrap()
            ),
            "composer" => self.final_parts.concat(),
            _ => unreachable!(),
        };
        self.calls.lock().unwrap().push(Call::Model {
            stage,
            messages,
            streaming,
        });
        let count = self.count.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behavior.lock().unwrap().get(&count).cloned();
        Box::pin(async move {
            match behavior {
                Some(Behavior::Fail(error)) => return Err(ModelFailure::Provider(error)),
                Some(Behavior::Text(text)) => return Ok(text),
                Some(Behavior::Cancel) => {
                    cancellation.cancel();
                }
                Some(Behavior::Pending(started, dropped)) => {
                    let _guard = DropFlag(dropped);
                    started.notify_one();
                    std::future::pending::<()>().await;
                }
                None => (),
            }
            Ok(response)
        })
    }
}

impl RagModel for FakeModel {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation) -> ModelFuture {
        self.response(messages, false, cancellation)
    }

    fn stream_final(&self, messages: Vec<Message>, context: RequestContext) -> ModelFuture {
        assert_eq!(
            stage(&messages),
            "composer",
            "private model stage was streamed"
        );
        let response = self.response(messages, true, context.cancellation());
        let parts = self.final_parts.clone();
        let error = self.stream_error;
        let pending = self.stream_pending.clone();
        Box::pin(async move {
            let reply = response.await?;
            for part in parts {
                context
                    .emit("delta", json!({"delta":part}))
                    .await
                    .map_err(ModelFailure::Backend)?;
            }
            if let Some((started, dropped)) = pending {
                let _guard = DropFlag(dropped);
                started.notify_one();
                std::future::pending::<()>().await;
            }
            if let Some(error) = error {
                return Err(ModelFailure::Provider(error));
            }
            Ok(reply)
        })
    }
}

impl ModelClient for FakeModel {
    fn complete(
        &self,
        messages: Vec<Message>,
        cancellation: Cancellation,
    ) -> BackendFuture<String> {
        let future = RagModel::complete(self, messages, cancellation);
        Box::pin(async move { future.await.map_err(coarse) })
    }
    fn stream(&self, messages: Vec<Message>, context: RequestContext) -> BackendFuture<String> {
        let future = RagModel::stream_final(self, messages, context);
        Box::pin(async move { future.await.map_err(coarse) })
    }
}

fn coarse(error: ModelFailure) -> ErrorCode {
    match error {
        ModelFailure::Provider(error) => error.into(),
        ModelFailure::Backend(error) => error,
    }
}

pub struct FakeRetriever {
    pub calls: Calls,
    pub results: HashMap<String, Vec<Chunk>>,
    pub behavior: Mutex<HashMap<usize, Behavior>>,
    pub count: AtomicUsize,
}

impl FakeRetriever {
    pub fn new(calls: Calls) -> Self {
        Self {
            calls,
            results: HashMap::new(),
            behavior: Mutex::default(),
            count: AtomicUsize::new(0),
        }
    }
}

impl Retriever for FakeRetriever {
    fn retrieve(
        &self,
        query: String,
        strategy: RetrievalStrategy,
        scope: RetrievalScope,
        limit: usize,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Chunk>> {
        self.calls.lock().unwrap().push(Call::Retrieve {
            query: query.clone(),
            strategy,
            scope,
            limit,
        });
        let results = self
            .results
            .get(&query)
            .cloned()
            .unwrap_or_else(|| vec![chunk(1, "shared", 0), chunk(2, &query, 0)]);
        let count = self.count.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behavior.lock().unwrap().get(&count).cloned();
        Box::pin(async move {
            match behavior {
                Some(Behavior::Fail(_)) => return Err(ErrorCode::Internal),
                Some(Behavior::Cancel) => {
                    cancellation.cancel();
                }
                Some(Behavior::Pending(started, dropped)) => {
                    let _guard = DropFlag(dropped);
                    started.notify_one();
                    std::future::pending::<()>().await;
                }
                _ => (),
            }
            Ok(results)
        })
    }
    fn search_history(
        &self,
        _: String,
        _: Option<String>,
        _: usize,
        _: Cancellation,
    ) -> BackendFuture<Value> {
        panic!("RAG must not access history")
    }
    fn index_exchange(&self, _: i64, _: Cancellation) -> BackendFuture<()> {
        panic!("RAG must never persist")
    }
}

pub fn chunk(rank: u32, paper: &str, index: u32) -> Chunk {
    Chunk {
        text: format!("evidence from {paper}"),
        source: SourceRecord {
            rank,
            paper_id: paper.into(),
            chunk_index: index,
            filename: format!("{paper}.pdf"),
            title: paper.into(),
            page: index + 1,
            score: 1.0 / f64::from(rank),
            snippet: "STALE_SNIPPET".into(),
        },
    }
}

pub fn stage(messages: &[Message]) -> &'static str {
    for (marker, stage) in [
        ("PLANNER", "planner"),
        ("SUBANSWER", "subanswer"),
        ("COVERAGE", "coverage"),
        ("COMPOSER", "composer"),
    ] {
        if messages
            .iter()
            .any(|m| m.content.starts_with(&format!("MULTIHOP_{marker}\n")))
        {
            return stage;
        }
    }
    panic!("missing stage")
}

pub fn payload(messages: &[Message]) -> Value {
    serde_json::from_str(&messages.last().unwrap().content).unwrap()
}

pub fn history() -> Vec<Message> {
    vec![Message {
        role: "human".into(),
        content: "earlier question".into(),
        exchange_id: Some(123),
        sources: vec![chunk(1, "old", 0).source],
    }]
}

pub fn fixtures(followups: &[&str]) -> (Arc<FakeModel>, Arc<FakeRetriever>, AdaptiveRag, Calls) {
    let calls = Calls::default();
    let model = Arc::new(FakeModel::new(
        calls.clone(),
        &["alpha evidence", "beta evidence"],
        followups,
    ));
    let retriever = Arc::new(FakeRetriever::new(calls.clone()));
    let pipeline = build(model.clone(), retriever.clone());
    (model, retriever, pipeline, calls)
}

pub fn build(model: Arc<dyn RagModel>, retriever: Arc<dyn Retriever>) -> AdaptiveRag {
    AdaptiveRag::with_model(model, retriever, SYSTEM.into()).unwrap()
}

pub async fn silent(pipeline: &AdaptiveRag) -> Result<RagRun, RagError> {
    pipeline
        .run_stateless(
            QUESTION.into(),
            history(),
            RunMode::Silent(Cancellation::default()),
        )
        .await
}

struct Handler {
    pipeline: AdaptiveRag,
    outcomes: mpsc::UnboundedSender<Result<RagRun, RagError>>,
}
impl CommandHandler for Handler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let pipeline = self.pipeline.clone();
        let outcomes = self.outcomes.clone();
        Box::pin(async move {
            if params["trait"] == true {
                let result =
                    RagPipeline::run(&pipeline, QUESTION.into(), history(), context).await?;
                return Ok(json!({"reply":result.reply,"sources":result.sources}));
            }
            let mode = if params["silent"] == true {
                RunMode::Silent(context.cancellation())
            } else {
                RunMode::Public(context)
            };
            let result = pipeline
                .run_stateless(QUESTION.into(), history(), mode)
                .await;
            let public = result
                .as_ref()
                .map(|run| json!({"reply":run.result.reply,"sources":run.result.sources}))
                .map_err(|error| (*error).into());
            let _ = outcomes.send(result);
            public
        })
    }
}

pub struct Runtime {
    writer: WriteHalf<DuplexStream>,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    task: JoinHandle<std::io::Result<()>>,
    pub outcomes: mpsc::UnboundedReceiver<Result<RagRun, RagError>>,
}

impl Runtime {
    pub fn new(pipeline: AdaptiveRag) -> Self {
        let (outcomes, receiver) = mpsc::unbounded_channel();
        let handler = Arc::new(Handler { pipeline, outcomes });
        let mut registry = Registry::new(
            ["chat_stream", "evaluate_stream"]
                .into_iter()
                .map(|command| CommandSpec {
                    command: command.into(),
                    owner: "rag-test".into(),
                    streaming: true,
                })
                .collect(),
        )
        .unwrap();
        for command in ["chat_stream", "evaluate_stream"] {
            registry
                .install(command, "rag-test", handler.clone())
                .unwrap();
        }
        let (client, server) = tokio::io::duplex(2 * 1024 * 1024);
        let (input, output) = tokio::io::split(server);
        let task = tokio::spawn(serve(input, output, registry));
        let (reader, writer) = tokio::io::split(client);
        Self {
            writer,
            reader: BufReader::new(reader).lines(),
            task,
            outcomes: receiver,
        }
    }
    pub async fn send(&mut self, input: Value) {
        self.writer
            .write_all(format!("{input}\n").as_bytes())
            .await
            .unwrap();
    }
    pub async fn request(&mut self, silent: bool) {
        let command = if silent {
            "evaluate_stream"
        } else {
            "chat_stream"
        };
        self.send(
            json!({"v":1,"type":"request","id":"rag","command":command,"params":{"silent":silent}}),
        )
        .await;
    }
    pub async fn next(&mut self) -> Value {
        serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }
    pub async fn terminal(&mut self) -> Vec<Value> {
        let mut values = vec![];
        loop {
            let value = self.next().await;
            let terminal = value["type"] != "event";
            values.push(value);
            if terminal {
                break;
            }
        }
        values
    }
    pub async fn finish(mut self) -> Vec<Value> {
        self.writer.shutdown().await.unwrap();
        let mut values = vec![];
        while let Some(line) = tokio::time::timeout(Duration::from_secs(3), self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
        {
            values.push(serde_json::from_str(&line).unwrap());
        }
        (&mut self.task).await.unwrap().unwrap();
        values
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.task.abort();
    }
}
