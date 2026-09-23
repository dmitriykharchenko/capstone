use research_backend::{
    chat::{NativeChat, install_commands},
    config::StoragePaths,
    domain::{Message, RagResult},
    history::SqliteHistory,
    indexing::CoordinatedIndex,
    protocol::ErrorCode,
    providers::EmbeddingClient,
    rag::RagPipeline,
    registry::{BackendFuture, Registry},
    retrieval::EmbeddingIdentity,
    runtime::{Cancellation, RequestContext, serve},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Semaphore,
};

struct Fake {
    histories: Mutex<Vec<(String, Vec<Message>)>>,
    models: AtomicUsize,
    embeddings: AtomicUsize,
    hold_model: AtomicBool,
    hold_embed: AtomicBool,
    fail_embed: AtomicBool,
    panic_embed: AtomicBool,
    model_entered: Semaphore,
    embed_entered: Semaphore,
    release_model: Semaphore,
    release_embed: Semaphore,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            histories: Mutex::new(vec![]),
            models: AtomicUsize::new(0),
            embeddings: AtomicUsize::new(0),
            hold_model: AtomicBool::new(false),
            hold_embed: AtomicBool::new(false),
            fail_embed: AtomicBool::new(false),
            panic_embed: AtomicBool::new(false),
            model_entered: Semaphore::new(0),
            embed_entered: Semaphore::new(0),
            release_model: Semaphore::new(0),
            release_embed: Semaphore::new(0),
        }
    }
}
struct Model(Arc<Fake>);
impl RagPipeline for Model {
    fn run(
        &self,
        question: String,
        history: Vec<Message>,
        context: RequestContext,
    ) -> BackendFuture<RagResult> {
        let f = self.0.clone();
        Box::pin(async move {
            f.models.fetch_add(1, Ordering::SeqCst);
            f.histories
                .lock()
                .unwrap()
                .push((question.clone(), history));
            f.model_entered.add_permits(1);
            if f.hold_model.load(Ordering::SeqCst) {
                f.release_model.acquire().await.unwrap().forget();
            }
            context.cancellation().check()?;
            if question == "fail" {
                return Err(ErrorCode::Internal);
            }
            assert!(question != "panic", "synthetic model panic");
            context
                .emit("delta", json!({"delta":"provisional"}))
                .await?;
            let reply = if question == "oversized" {
                "x".repeat(1_048_576)
            } else {
                format!("answer:{question}")
            };
            Ok(RagResult {
                reply,
                sources: vec![],
                chunks: vec![],
            })
        })
    }
}
struct Embed(Arc<Fake>);
impl EmbeddingClient for Embed {
    fn embed(&self, texts: Vec<String>, c: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
        let f = self.0.clone();
        Box::pin(async move {
            f.embeddings.fetch_add(1, Ordering::SeqCst);
            f.embed_entered.add_permits(1);
            if f.hold_embed.load(Ordering::SeqCst) {
                f.release_embed.acquire().await.unwrap().forget();
            }
            c.check()?;
            assert!(
                !f.panic_embed.load(Ordering::SeqCst),
                "synthetic embedding panic"
            );
            if f.fail_embed.load(Ordering::SeqCst) {
                return Err(ErrorCode::Internal);
            }
            Ok(texts.iter().map(|_| vec![0.25, 0.75]).collect())
        })
    }
}
fn paths(root: &std::path::Path) -> StoragePaths {
    let data = root.join("data");
    let papers = root.join("papers");
    let resources = root.join("resources");
    for p in [&data, &papers, &resources] {
        std::fs::create_dir_all(p).unwrap();
    }
    StoragePaths {
        history_database: data.join("history.sqlite3"),
        vector_database: data.join("vectors.sqlite3"),
        graph_database: data.join("graph.sqlite3"),
        index_state_database: data.join("index-state.sqlite3"),
        bm25_index: data.join("bm25.json"),
        evaluation_results: data.join("evaluations"),
        data_root: data,
        papers,
        resources,
    }
}
async fn setup(
    paths: &StoragePaths,
    f: Arc<Fake>,
) -> (SqliteHistory, CoordinatedIndex, NativeChat) {
    let history = SqliteHistory::new(&paths.data_root, &paths.history_database).unwrap();
    history.initialize(&Cancellation::default()).unwrap();
    let index = CoordinatedIndex::open(
        paths,
        EmbeddingIdentity {
            model: "fake/v1".into(),
            dimension: 2,
        },
    )
    .await
    .unwrap()
    .with_history(history.clone());
    index.recover().await.unwrap();
    let retriever = index.retriever(Arc::new(Embed(f.clone())));
    let chat = NativeChat::new(
        history.clone(),
        index.clone(),
        retriever,
        Arc::new(Model(f)),
    );
    (history, index, chat)
}
type Lines = tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>;
struct Client {
    lines: Lines,
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    seq: usize,
}
struct Reset(CoordinatedIndex);
impl research_backend::registry::CommandHandler for Reset {
    fn cancellation_mode(&self) -> research_backend::runtime::CancellationMode {
        research_backend::runtime::CancellationMode::Cooperative
    }
    fn call(&self, _: Value, c: RequestContext) -> BackendFuture<Value> {
        let index = self.0.clone();
        Box::pin(async move {
            index.reset(&c.operation_group()?).await?;
            Ok(json!({"ok":true}))
        })
    }
}
impl Client {
    fn new(history: SqliteHistory, chat: NativeChat) -> Self {
        Self::with_reset(history, chat, None)
    }
    fn with_reset(
        history: SqliteHistory,
        chat: NativeChat,
        index: Option<CoordinatedIndex>,
    ) -> Self {
        let mut registry = Registry::foundation();
        install_commands(&mut registry, history, Some(chat));
        if let Some(index) = index {
            registry
                .install("reset_all_indices", "indexing", Arc::new(Reset(index)))
                .unwrap();
        }
        let (client, server) = tokio::io::duplex(2 * 1024 * 1024);
        let (r, w) = tokio::io::split(server);
        let server = tokio::spawn(serve(r, w, registry));
        let (read, write) = tokio::io::split(client);
        Self {
            lines: BufReader::new(read).lines(),
            write,
            server,
            seq: 0,
        }
    }
    async fn send(&mut self, command: &str, params: Value) -> String {
        self.seq += 1;
        let id = format!("r{}", self.seq);
        self.raw(json!({"v":1,"type":"request","id":id,"command":command,"params":params}))
            .await;
        id
    }
    async fn raw(&mut self, value: Value) {
        self.write
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), self.lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn terminal(&mut self) -> Value {
        loop {
            let frame = self.next().await;
            if frame["type"] != "event" {
                return frame;
            }
        }
    }
    async fn call(&mut self, command: &str, params: Value) -> Value {
        self.send(command, params).await;
        self.terminal().await
    }
    async fn finish(mut self) {
        self.write.shutdown().await.unwrap();
        self.server.await.unwrap().unwrap();
    }
}
fn turn(id: &str, session: &str, message: &str) -> Value {
    json!({"turn_id":id,"session_id":session,"message":message})
}
async fn entered(s: &Semaphore) {
    tokio::time::timeout(std::time::Duration::from_secs(10), s.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

#[tokio::test]
async fn native_chat_roundtrip_sessions_search_duplicate_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    {
        let (h, index, chat) = setup(&p, f.clone()).await;
        let mut c = Client::new(h.clone(), chat);
        let response = c.call("chat_stream", turn("t1", "s", "one")).await;
        assert_eq!(response["type"], "result", "{response}");
        let data = response["data"].clone();
        assert_eq!(data["reply"], "answer:one");
        assert_eq!(data["indexed"], true);
        assert_eq!(data["exchange_id"], 1);
        assert_eq!(c.call("chat", turn("t1", "s", "one")).await["data"], data);
        for request in [turn("t1", "s", "other"), turn("t1", "other", "one")] {
            assert_eq!(
                c.call("chat", request).await["error"]["code"],
                "invalid_params"
            );
        }
        assert_eq!(f.models.load(Ordering::SeqCst), 1);
        assert_eq!(f.embeddings.load(Ordering::SeqCst), 1);
        assert_eq!(
            c.call("list_sessions", json!({})).await["data"]["sessions"][0]["message_count"],
            2
        );
        let messages = c.call("get_messages", json!({"session_id":"s"})).await;
        assert_eq!(messages["data"]["messages"][1]["turn_id"], "t1");
        assert_eq!(
            messages["data"]["messages"][1]["history_index_status"],
            "indexed"
        );
        assert_eq!(
            c.call("search", json!({"query":"one","session_id":"s"}))
                .await["data"]["matches"][0]["assistant_message"],
            "answer:one"
        );
        assert_eq!(
            c.call("search", json!({"query":"one","session_id":"other"}))
                .await["data"]["matches"],
            json!([])
        );
        index.recover().await.unwrap();
        c.finish().await;
    }
    let model_calls = f.models.load(Ordering::SeqCst);
    let embed_calls = f.embeddings.load(Ordering::SeqCst);
    let (h, _index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h, chat);
    assert_eq!(
        c.call("chat", turn("t1", "s", "one")).await["data"]["exchange_id"],
        1
    );
    assert_eq!(f.models.load(Ordering::SeqCst), model_calls);
    assert_eq!(f.embeddings.load(Ordering::SeqCst), embed_calls);
    c.finish().await;
}

#[tokio::test]
async fn incomplete_model_runs_never_publish_and_same_id_never_replays() {
    for question in ["fail", "panic", "oversized"] {
        let root = tempfile::tempdir().unwrap();
        let p = paths(root.path());
        let f = Arc::new(Fake::default());
        let (h, _index, chat) = setup(&p, f.clone()).await;
        let mut c = Client::new(h.clone(), chat);
        assert_eq!(
            c.call("chat_stream", turn("t", "s", question)).await["type"],
            "error"
        );
        assert!(h.get_messages("s").unwrap().is_empty());
        assert!(h.list_exchange_index_intents(0, 256).unwrap().is_empty());
        assert_eq!(
            c.call("chat", turn("t", "s", question)).await["error"]["code"],
            "outcome_unknown"
        );
        assert_eq!(f.models.load(Ordering::SeqCst), 1);
        assert_eq!(f.embeddings.load(Ordering::SeqCst), 0);
        c.finish().await;
    }
}

#[tokio::test]
async fn embedding_failure_preserves_saved_reply_and_no_paid_retry() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.fail_embed.store(true, Ordering::SeqCst);
    let (h, index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    let result = c.call("chat", turn("t", "s", "one")).await;
    assert_eq!(result["type"], "result", "{result}");
    assert_eq!(result["data"]["indexed"], false);
    assert_eq!(result["data"]["history_index_status"], "failed");
    assert!(
        result["data"]["warning"]
            .as_str()
            .unwrap()
            .contains("saved")
    );
    assert_eq!(h.get_messages("s").unwrap().len(), 2);
    index.recover().await.unwrap();
    assert_eq!(
        c.call("chat", turn("t", "s", "one")).await["data"]["reply"],
        "answer:one"
    );
    assert_eq!(f.models.load(Ordering::SeqCst), 1);
    assert_eq!(f.embeddings.load(Ordering::SeqCst), 1);
    c.finish().await;
}

#[tokio::test]
async fn cancel_during_rag_drains_and_never_publishes() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.hold_model.store(true, Ordering::SeqCst);
    let (h, _index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    let id = c.send("chat_stream", turn("t", "s", "one")).await;
    entered(&f.model_entered).await;
    c.raw(json!({"v":1,"type":"cancel","id":id})).await;
    assert_eq!(c.next().await["accepted"], true);
    assert!(h.get_messages("s").unwrap().is_empty());
    f.release_model.add_permits(1);
    assert_eq!(c.terminal().await["type"], "cancelled");
    assert!(h.get_messages("s").unwrap().is_empty());
    assert_eq!(f.embeddings.load(Ordering::SeqCst), 0);
    c.finish().await;
}

#[tokio::test]
async fn post_commit_cancel_is_refused_and_eof_drains_embedding() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.hold_embed.store(true, Ordering::SeqCst);
    let (h, _index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    let id = c.send("chat", turn("t", "s", "one")).await;
    entered(&f.embed_entered).await;
    assert_eq!(h.get_messages("s").unwrap().len(), 2);
    c.raw(json!({"v":1,"type":"cancel","id":id})).await;
    assert_eq!(c.next().await["accepted"], false);
    c.write.shutdown().await.unwrap();
    f.release_embed.add_permits(1);
    assert_eq!(c.terminal().await["type"], "result");
    c.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn duplicate_and_fifo_turns_have_one_owner_and_ordered_history() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.hold_model.store(true, Ordering::SeqCst);
    let (h, _index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    c.send("chat", turn("t1", "s", "one")).await;
    entered(&f.model_entered).await;
    c.send("chat", turn("t1", "s", "one")).await;
    c.send("chat", turn("t2", "s", "two")).await;
    f.hold_model.store(false, Ordering::SeqCst);
    f.release_model.add_permits(1);
    for _ in 0..3 {
        let response = c.terminal().await;
        assert_eq!(response["type"], "result", "{response}");
    }
    assert_eq!(f.models.load(Ordering::SeqCst), 2);
    assert_eq!(f.embeddings.load(Ordering::SeqCst), 2);
    {
        let histories = f.histories.lock().unwrap();
        assert_eq!(histories[1].1.len(), 2);
        assert_eq!(histories[1].1[1].content, "answer:one");
    }
    assert_eq!(h.get_messages("s").unwrap().len(), 4);
    c.finish().await;
}

#[tokio::test]
async fn different_sessions_overlap_without_misrouting() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.hold_model.store(true, Ordering::SeqCst);
    let (h, _index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    c.send("chat", turn("t1", "a", "one")).await;
    c.send("chat", turn("t2", "b", "two")).await;
    entered(&f.model_entered).await;
    entered(&f.model_entered).await;
    f.release_model.add_permits(2);
    for _ in 0..2 {
        assert_eq!(c.terminal().await["type"], "result");
    }
    assert_eq!(h.get_messages("a").unwrap()[1].content, "answer:one");
    assert_eq!(h.get_messages("b").unwrap()[1].content, "answer:two");
    c.finish().await;
}

#[tokio::test]
async fn embedding_panic_is_contained_but_unverifiable_canonical_history_is_unknown() {
    for corrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let p = paths(root.path());
        let f = Arc::new(Fake::default());
        f.panic_embed.store(true, Ordering::SeqCst);
        f.hold_embed.store(corrupt, Ordering::SeqCst);
        let (h, _index, chat) = setup(&p, f.clone()).await;
        let mut c = Client::new(h, chat);
        c.send("chat", turn("t", "s", "one")).await;
        if corrupt {
            entered(&f.embed_entered).await;
            sqlite::open(&p.history_database)
                .unwrap()
                .execute("UPDATE messages SET content='changed' WHERE role='assistant'")
                .unwrap();
            f.release_embed.add_permits(1);
        }
        let result = c.terminal().await;
        if corrupt {
            assert_eq!(result["error"]["code"], "outcome_unknown", "{result}");
        } else {
            assert_eq!(result["type"], "result");
            assert_eq!(result["data"]["indexed"], false);
        }
        assert_eq!(f.embeddings.load(Ordering::SeqCst), 1);
        c.finish().await;
    }
}

#[tokio::test]
async fn journal_failure_after_vector_commit_blocks_derived_reads_but_not_saved_history() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.hold_embed.store(true, Ordering::SeqCst);
    let (h, index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    c.send("chat", turn("t", "s", "one")).await;
    entered(&f.embed_entered).await;
    let db = sqlite::open(&p.index_state_database).unwrap();
    db.execute("CREATE TRIGGER finish_fault BEFORE UPDATE ON exchange_entries BEGIN SELECT RAISE(ABORT,'synthetic journal failure'); END").unwrap();
    f.release_embed.add_permits(1);
    let response = c.terminal().await;
    assert_eq!(response["type"], "result", "{response}");
    assert_eq!(response["data"]["history_index_status"], "blocked");
    assert_eq!(
        c.call("get_messages", json!({"session_id":"s"})).await["data"]["messages"][1]["content"],
        "answer:one"
    );
    assert_eq!(
        c.call("search", json!({"query":"one"})).await["type"],
        "error"
    );
    db.execute("DROP TRIGGER finish_fault").unwrap();
    drop(db);
    index.recover().await.unwrap();
    assert_eq!(
        c.call("chat", turn("t", "s", "one")).await["data"]["indexed"],
        true
    );
    assert_eq!(f.models.load(Ordering::SeqCst), 1);
    assert_eq!(f.embeddings.load(Ordering::SeqCst), 1);
    c.finish().await;
}

#[tokio::test]
async fn reset_during_embedding_cannot_publish_an_old_epoch_vector() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    f.hold_embed.store(true, Ordering::SeqCst);
    let (h, index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::with_reset(h.clone(), chat, Some(index.clone()));
    c.send("chat", turn("t", "s", "one")).await;
    entered(&f.embed_entered).await;
    let reset = c.call("reset_all_indices", json!({})).await;
    assert_eq!(reset["type"], "result", "{reset}");
    f.release_embed.add_permits(1);
    let result = c.terminal().await;
    assert_eq!(result["type"], "result");
    assert_eq!(result["data"]["indexed"], false);
    assert_eq!(
        result["data"]["history_index_status_observed_at"]["reset_epoch"],
        1
    );
    assert_eq!(h.get_messages("s").unwrap().len(), 2);
    assert!(!index.observe_exchange(1).await.unwrap().indexed);
    assert_eq!(f.embeddings.load(Ordering::SeqCst), 1);
    c.finish().await;
}

#[tokio::test]
async fn missing_canonical_record_fails_search_and_poisoned_gate_keeps_other_sessions_readable() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let f = Arc::new(Fake::default());
    let (h, _index, chat) = setup(&p, f.clone()).await;
    let mut c = Client::new(h.clone(), chat);
    assert_eq!(
        c.call("chat", turn("t", "s", "one")).await["type"],
        "result"
    );
    sqlite::open(&p.history_database)
        .unwrap()
        .execute("DELETE FROM messages WHERE exchange_id=1")
        .unwrap();
    assert_eq!(
        c.call("search", json!({"query":"one"})).await["type"],
        "error"
    );
    assert_eq!(
        c.call("get_messages", json!({"session_id":"other"})).await["data"]["messages"],
        json!([])
    );
    assert_eq!(
        c.call("chat", turn("t", "s", "one")).await["error"]["code"],
        "outcome_unknown"
    );
    c.finish().await;
}
