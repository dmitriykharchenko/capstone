// Included by retrieval's cfg(test) module so the SQLite synchronization seam
// stays private and absent from production. Run: cargo test --lib rag_native_cancellation.
#[allow(unused_macros)]
macro_rules! rag_native_cancellation_tests {
() => {
use crate::{
    domain::Message,
    protocol::{ErrorCode, MAX_ACTIVE_REQUESTS},
    providers::EmbeddingClient,
    rag::{AdaptiveRag, ModelFuture, RagModel, RagPipeline, RunMode},
    registry::{BackendFuture, CommandHandler, Registry},
    retrieval::{EmbeddedChunk, EmbeddingIdentity, NativeRetriever, PreparedOperation, PreparedPaper, RetrievalError, VectorStore},
    runtime::{Cancellation, RequestContext, serve},
};
use futures_util::task::AtomicWaker;
use serde_json::{Value, json};
use std::{cell::RefCell, io, path::Path, pin::Pin, sync::{Arc, Condvar, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}}, task::{Context, Poll}, time::Duration};
use tokio::{io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadBuf, ReadHalf, WriteHalf}, sync::{Notify, Semaphore, oneshot}};

const DEADLINE: Duration = Duration::from_secs(5);
async fn bounded<T>(f: impl std::future::Future<Output=T>) -> T {
    tokio::time::timeout(DEADLINE, f).await.expect("test synchronization timed out")
}

struct Exit(Arc<AtomicUsize>);
impl Drop for Exit {
    fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
}
thread_local! { static THREAD_EXIT: RefCell<Option<Exit>> = const { RefCell::new(None) }; }

struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: Semaphore,
    exited: Arc<AtomicUsize>,
    tokens: Mutex<Vec<Cancellation>>,
    panic: bool,
}
impl Gate {
    fn new(panic: bool) -> Arc<Self> {
        Arc::new(Self { open: Mutex::new(false), changed: Condvar::new(), entered: Semaphore::new(0), exited: Arc::default(), tokens: Mutex::default(), panic })
    }
    fn hold(&self, db: &sqlite::Connection, c: &Cancellation) {
        // BEGIN alone is deferred: perform a real read to hold a SQLite SHARED
        // lock, proving the worker owns native resources at the test barrier.
        db.execute("SELECT * FROM retrieval_meta").unwrap();
        THREAD_EXIT.with(|slot| *slot.borrow_mut() = Some(Exit(self.exited.clone())));
        self.tokens.lock().unwrap().push(c.clone());
        self.entered.add_permits(1);
        let (open, _) = self.changed.wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| !*open).unwrap();
        assert!(*open, "native test barrier was not released");
        drop(open);
        assert!(!self.panic, "synthetic native panic");
    }
    async fn started(&self, n: usize) {
        bounded(self.entered.acquire_many(n as u32)).await.unwrap().forget();
    }
    async fn cancelled(&self) {
        bounded(async {
            loop {
                if self.tokens.lock().unwrap().iter().all(|c| c.check().is_err()) { break; }
                tokio::task::yield_now().await;
            }
        }).await;
    }
    fn release(&self) { *self.open.lock().unwrap() = true; self.changed.notify_all(); }
}

struct Embeddings;
impl EmbeddingClient for Embeddings {
    fn embed(&self, texts: Vec<String>, c: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
        Box::pin(async move { c.check()?; Ok(vec![vec![0.0]; texts.len()]) })
    }
}
fn fixture(gate: Option<Arc<Gate>>) -> (tempfile::TempDir, NativeRetriever) {
    let temp = tempfile::tempdir().unwrap();
    let identity = EmbeddingIdentity { model: "offline-test".into(), dimension: 1 };
    let mut store = VectorStore::new(temp.path(), &temp.path().join("vectors.sqlite"), identity.clone()).unwrap();
    store.initialize(&Cancellation::default()).unwrap();
    let record = EmbeddedChunk { id: "p:0".into(), paper_id: "p".into(), chunk_index: 0, filename: "p.pdf".into(), title: "Paper".into(), page: 1, text: "alpha beta evidence".into(), vector: vec![0.0] };
    store.apply(&[PreparedOperation::ReplacePaper(PreparedPaper::new("p".into(), identity, vec![record]).unwrap())], None, &Cancellation::default()).unwrap();
    if let Some(gate) = gate { store.snapshot_hook = Some(Arc::new(move |db, c| gate.hold(db, c))); }
    (temp, NativeRetriever::new(store, Arc::new(Embeddings), None))
}
fn lock_available(temp: &Path, available: bool) {
    let db = sqlite::open(temp.join("vectors.sqlite")).unwrap();
    assert_eq!(db.execute("BEGIN EXCLUSIVE").is_ok(), available, "native SQLite read lock lifetime");
}

struct Model { pending: bool, final_started: Arc<Notify> }
impl RagModel for Model {
    fn complete(&self, messages: Vec<Message>, _: Cancellation) -> ModelFuture {
        // Respond to all private stages with the required shapes; non-JSON text
        // is only required by subanswers, which also accept these finite strings.
        let planner = messages.iter().any(|m| m.content.contains("MULTIHOP_PLANNER"));
        let coverage = messages.iter().any(|m| m.content.contains("MULTIHOP_COVERAGE"));
        Box::pin(async move { Ok(if planner { json!({"subqueries":["alpha", "beta"]}).to_string() } else if coverage { json!({"complete":true,"follow_up_subqueries":[]}).to_string() } else { "evidence".into() }) })
    }
    fn stream_final(&self, _: Vec<Message>, context: RequestContext) -> ModelFuture {
        let pending = self.pending;
        let started = self.final_started.clone();
        Box::pin(async move {
            context.emit("delta", json!({"delta":"provisional"})).await.map_err(crate::rag::ModelFailure::Backend)?;
            started.notify_one();
            if pending { std::future::pending::<()>().await; }
            Ok("authoritative".into())
        })
    }
}
fn pipeline(native: NativeRetriever, pending: bool) -> AdaptiveRag {
    AdaptiveRag::with_model(Arc::new(Model { pending, final_started: Arc::default() }), Arc::new(native), "offline".into()).unwrap()
}
struct Handler(AdaptiveRag);
impl CommandHandler for Handler {
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        let pipeline = self.0.clone();
        Box::pin(async move {
            let result = pipeline.run("question".into(), vec![], context).await?;
            Ok(json!({"reply":result.reply, "sources":result.sources}))
        })
    }
}

#[derive(Default)]
struct Control { fail: AtomicBool, stall: AtomicBool, observed: Notify, waker: AtomicWaker }
struct Reader(ReadHalf<DuplexStream>, Arc<Control>);
impl AsyncRead for Reader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.1.waker.register(cx.waker());
        if self.1.fail.load(Ordering::SeqCst) { return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into())); }
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
struct Writer(WriteHalf<DuplexStream>, Arc<Control>, Option<Arc<Gate>>);
impl AsyncWrite for Writer {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.1.waker.register(cx.waker());
        if self.1.fail.load(Ordering::SeqCst) { self.1.observed.notify_one(); return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())); }
        if self.1.stall.load(Ordering::SeqCst) { self.1.observed.notify_one(); return Poll::Pending; }
        let frame: Value = serde_json::from_slice(buf).unwrap();
        if frame["type"] == "cancelled" {
            if let Some(gate) = &self.2 {
                assert!(*gate.open.lock().unwrap(), "cancelled published before native barrier release");
                assert!(gate.exited.load(Ordering::SeqCst) > 0, "cancelled published before native thread exit");
            }
        }
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { Pin::new(&mut self.0).poll_flush(cx) }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { Pin::new(&mut self.0).poll_shutdown(cx) }
}
struct Client {
    write: WriteHalf<DuplexStream>, read: Lines<BufReader<ReadHalf<DuplexStream>>>,
    task: tokio::task::JoinHandle<io::Result<()>>, input: Arc<Control>, output: Arc<Control>,
}
impl Client {
    fn new(native: NativeRetriever, gate: Option<Arc<Gate>>, pending: bool) -> Self {
        Self::handler(Arc::new(Handler(pipeline(native, pending))), gate)
    }
    fn handler(handler: Arc<dyn CommandHandler>, gate: Option<Arc<Gate>>) -> Self {
        let mut registry = Registry::foundation();
        registry.install("chat_stream", "chat", handler).unwrap();
        let (client, server) = tokio::io::duplex(2 * 1024 * 1024);
        let (r, w) = tokio::io::split(server);
        let input = Arc::new(Control::default()); let output = Arc::new(Control::default());
        let task = tokio::spawn(serve(Reader(r, input.clone()), Writer(w, output.clone(), gate), registry));
        let (r, write) = tokio::io::split(client);
        Self { write, read: BufReader::new(r).lines(), task, input, output }
    }
    async fn send(&mut self, v: Value) { self.write.write_all(format!("{v}\n").as_bytes()).await.unwrap(); }
    async fn request(&mut self, id: &str, command: &str) { self.send(json!({"v":1,"type":"request","id":id,"command":command,"params":{}})).await; }
    async fn cancel(&mut self, id: &str) { self.send(json!({"v":1,"type":"cancel","id":id})).await; }
    async fn next(&mut self) -> Value { serde_json::from_str(&bounded(self.read.next_line()).await.unwrap().unwrap()).unwrap() }
    async fn until(&mut self, kind: &str) -> Value {
        loop { let v = self.next().await; if v["type"] == kind { return v; } assert_eq!(v["type"], "event"); }
    }
    async fn finish(mut self) {
        self.write.shutdown().await.unwrap();
        assert!(bounded(self.read.next_line()).await.unwrap().is_none(), "duplicate terminal/late event");
        bounded(self.task).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn real_snapshot_drop_and_task_abort_join_before_return() {
    for abort in [false, true] {
        let gate = Gate::new(false); let (temp, native) = fixture(Some(gate.clone()));
        let (stop, stopped) = oneshot::channel(); let (done, mut finished) = oneshot::channel();
        let token = Cancellation::default(); let check = token.clone();
        // A separate executor deliberately allows a blocking Drop; its release
        // is controlled by this executor, never by the executor being joined.
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                if abort {
                    let job = tokio::spawn(async move { native.snapshot(token).await });
                    stopped.await.unwrap(); job.abort(); assert!(job.await.unwrap_err().is_cancelled());
                } else {
                    let mut future = Box::pin(native.snapshot(token));
                    tokio::select! { _ = &mut future => panic!("snapshot unexpectedly finished"), _ = stopped => {} }
                    drop(future);
                }
            });
            done.send(()).unwrap();
        });
        gate.started(1).await; lock_available(temp.path(), false);
        stop.send(()).unwrap(); gate.cancelled().await;
        assert_eq!(check.check(), Err(ErrorCode::Cancelled));
        assert!(matches!(finished.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        gate.release(); bounded(finished).await.unwrap(); thread.join().unwrap();
        assert_eq!(gate.exited.load(Ordering::SeqCst), 1); lock_available(temp.path(), true);
    }
}

#[tokio::test]
async fn cancelled_real_rag_keeps_capacity_and_health_responsive_until_native_teardown() {
    let gate = Gate::new(false); let (temp, native) = fixture(Some(gate.clone()));
    let mut client = Client::new(native, Some(gate.clone()), false);
    for i in 0..MAX_ACTIVE_REQUESTS { client.request(&format!("job{i}"), "chat_stream").await; }
    gate.started(MAX_ACTIVE_REQUESTS).await; lock_available(temp.path(), false);
    for i in 0..MAX_ACTIVE_REQUESTS {
        client.cancel(&format!("job{i}")).await;
        assert_eq!(client.until("cancel_ack").await["accepted"], true);
    }
    client.cancel("job0").await;
    assert_eq!(client.next().await["accepted"], true, "repeat cancel while draining is idempotent");
    client.request("overflow", "chat_stream").await;
    assert_eq!(client.next().await["error"]["code"], "busy");
    client.request("health", "health").await;
    assert_eq!(client.next().await["data"]["ok"], true);
    gate.cancelled().await; assert_eq!(gate.exited.load(Ordering::SeqCst), 0);
    gate.release();
    let mut ids = std::collections::HashSet::new();
    for _ in 0..MAX_ACTIVE_REQUESTS {
        let terminal = client.next().await; assert_eq!(terminal["type"], "cancelled");
        assert!(ids.insert(terminal["id"].as_str().unwrap().to_owned()));
    }
    assert_eq!(gate.exited.load(Ordering::SeqCst), MAX_ACTIVE_REQUESTS); lock_available(temp.path(), true);
    client.cancel("job0").await; assert_eq!(client.next().await["accepted"], false);
    client.finish().await;
}

#[tokio::test]
async fn real_rag_eof_read_error_and_output_loss_drain_native_before_return() {
    for mode in ["eof", "read", "output", "cancel_then_eof", "blocked_output"] {
        let gate = Gate::new(false); let (temp, native) = fixture(Some(gate.clone()));
        let mut client = Client::new(native, Some(gate.clone()), false);
        client.request("job", "chat_stream").await; gate.started(1).await;
        // Consume all pre-retrieval progress; then force the selected transport path.
        assert_eq!(client.next().await["event"], "plan"); assert_eq!(client.next().await["event"], "subquery");
        match mode {
            "eof" => client.write.shutdown().await.unwrap(),
            "cancel_then_eof" => { client.cancel("job").await; assert_eq!(client.next().await["accepted"], true); client.write.shutdown().await.unwrap(); }
            "read" => { client.input.fail.store(true, Ordering::SeqCst); client.input.waker.wake(); }
            "output" => { client.output.fail.store(true, Ordering::SeqCst); client.request("probe", "health").await; bounded(client.output.observed.notified()).await; }
            _ => {
                client.output.stall.store(true, Ordering::SeqCst); client.request("probe", "health").await; bounded(client.output.observed.notified()).await;
                // Lost writer while output is blocked wakes the real write path.
                client.output.fail.store(true, Ordering::SeqCst); client.output.waker.wake();
            }
        }
        gate.cancelled().await; assert!(!client.task.is_finished()); lock_available(temp.path(), false);
        gate.release();
        let outcome = bounded(client.task).await.unwrap();
        assert_eq!(outcome.is_ok(), matches!(mode, "eof" | "cancel_then_eof"));
        assert_eq!(gate.exited.load(Ordering::SeqCst), 1); lock_available(temp.path(), true);
        let mut terminals = 0;
        while let Some(line) = bounded(client.read.next_line()).await.unwrap() {
            let v: Value = serde_json::from_str(&line).unwrap(); assert_eq!(v["type"], "cancelled"); terminals += 1;
        }
        assert_eq!(terminals, usize::from(!matches!(mode, "output" | "blocked_output")));
    }
}

#[tokio::test]
async fn native_panic_is_sanitized_and_releases_snapshot_before_terminal() {
    let gate = Gate::new(true); let (temp, native) = fixture(Some(gate.clone()));
    let mut client = Client::new(native, Some(gate.clone()), false);
    client.request("job", "chat_stream").await; gate.started(1).await; gate.release();
    let terminal = client.until("error").await;
    assert_eq!(terminal["error"]["code"], "internal");
    assert!(!terminal.to_string().contains("synthetic"));
    assert_eq!(gate.exited.load(Ordering::SeqCst), 1); lock_available(temp.path(), true);
    client.finish().await;
}

#[tokio::test]
async fn actual_native_rag_final_deltas_remain_provisional_and_result_is_authoritative() {
    for pending in [true, false] {
        let (temp, native) = fixture(None);
        let mut client = Client::new(native, None, pending);
        client.request("job", "chat_stream").await;
        loop { let event = client.next().await; assert_eq!(event["type"], "event"); if event["event"] == "delta" { assert_eq!(event["data"]["delta"], "provisional"); break; } }
        if pending {
            client.cancel("job").await; assert_eq!(client.next().await["accepted"], true);
            assert_eq!(client.next().await["type"], "cancelled");
        } else {
            assert_eq!(client.next().await["data"]["reply"], "authoritative");
            client.cancel("job").await; assert_eq!(client.next().await["accepted"], false);
        }
        lock_available(temp.path(), true); client.finish().await;
    }
}

#[tokio::test]
async fn silent_rag_cancellation_joins_real_native_retrieval() {
    let gate = Gate::new(false); let (temp, native) = fixture(Some(gate.clone()));
    let token = Cancellation::default(); let c = token.clone(); let rt = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || rt.block_on(pipeline(native, false).run_stateless("question".into(), vec![], RunMode::Silent(c))));
    gate.started(1).await; token.cancel(); gate.cancelled().await;
    assert!(!task.is_finished()); gate.release();
    assert!(bounded(task).await.unwrap().is_err());
    assert_eq!(gate.exited.load(Ordering::SeqCst), 1); lock_available(temp.path(), true);
}

struct Mutation {
    native: NativeRetriever,
    ready: Arc<Notify>,
    publish: Arc<Semaphore>,
    token: Arc<Mutex<Option<Cancellation>>>,
}
impl CommandHandler for Mutation {
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        let native = self.native.clone(); let ready = self.ready.clone(); let publish = self.publish.clone(); let token = self.token.clone();
        Box::pin(async move {
            *token.lock().unwrap() = Some(context.cancellation());
            let outcome = native.apply(vec![PreparedOperation::DeletePaper("p".into())], None, context.cancellation()).await;
            ready.notify_one(); publish.acquire().await.unwrap().forget();
            // Flood the bounded queue after seal. Even a lost output receiver
            // must drain this queue rather than strand the admitted request.
            for _ in 0..crate::protocol::QUEUE_CAPACITY * 3 { context.emit("progress", json!({})).await?; }
            outcome.map(|s| json!({"chunks":s.chunk_count})).map_err(Into::into)
        })
    }
}
#[tokio::test]
async fn real_native_apply_sealed_success_and_failure_survive_cancel_eof_and_output_loss() {
    for fail in [false, true] {
        for output_loss in [false, true] {
            let (temp, native) = fixture(None);
            let reader = sqlite::open(temp.path().join("vectors.sqlite")).unwrap();
            if fail { reader.execute("BEGIN; SELECT * FROM chunks").unwrap(); }
            let ready = Arc::new(Notify::new()); let publish = Arc::new(Semaphore::new(0)); let token = Arc::new(Mutex::new(None));
            let mut client = Client::handler(Arc::new(Mutation { native: native.clone(), ready: ready.clone(), publish: publish.clone(), token: token.clone() }), None);
            client.request("job", "chat_stream").await; bounded(ready.notified()).await;
            assert!(!token.lock().unwrap().as_ref().unwrap().cancel(), "actual COMMIT outcome retains seal");
            client.cancel("job").await; assert_eq!(client.next().await["accepted"], false);
            drop(reader);
            if output_loss {
                client.output.fail.store(true, Ordering::SeqCst); client.request("probe", "health").await; bounded(client.output.observed.notified()).await;
            } else { client.write.shutdown().await.unwrap(); }
            assert!(!client.task.is_finished()); publish.add_permits(1);
            if !output_loss {
                let terminal = client.until(if fail { "error" } else { "result" }).await;
                if fail { assert_eq!(terminal["error"]["code"], "internal"); } else { assert_eq!(terminal["data"]["chunks"], 0); }
            }
            assert_eq!(bounded(client.task).await.unwrap().is_err(), output_loss);
            assert!(bounded(client.read.next_line()).await.unwrap().is_none());
            assert_eq!(native.status(Cancellation::default()).await.unwrap().chunk_count, usize::from(fail));
            lock_available(temp.path(), true);
        }
    }
}

#[tokio::test]
async fn real_native_error_remains_typed_and_worker_releases_storage() {
    let (temp, native) = fixture(None);
    let db = sqlite::open(temp.path().join("vectors.sqlite")).unwrap();
    db.execute("PRAGMA user_version=999").unwrap(); drop(db);
    assert_eq!(native.snapshot(Cancellation::default()).await.unwrap_err(), RetrievalError::InvalidStore);
    lock_available(temp.path(), true);
}
};
}
