use futures_util::task::AtomicWaker;
use research_backend::{
    protocol::{ErrorCode, MAX_FRAME_BYTES, QUEUE_CAPACITY},
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{Cancellation, RequestContext, serve},
};
use serde_json::{Value, json};
use std::{
    io,
    pin::Pin,
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{
        AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, Lines,
        ReadBuf, ReadHalf, WriteHalf,
    },
    sync::{Notify, Semaphore},
};

const DEADLINE: Duration = Duration::from_secs(3);

async fn notified(signal: &Notify) {
    tokio::time::timeout(DEADLINE, signal.notified())
        .await
        .expect("test phase timed out");
}

#[derive(Default)]
struct NativeGate(Mutex<bool>, Condvar);
impl NativeGate {
    fn wait(&self) {
        let (open, _) = self
            .1
            .wait_timeout_while(self.0.lock().unwrap(), DEADLINE, |open| !*open)
            .unwrap();
        assert!(*open, "native test gate timed out");
    }
    fn release(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
}

struct Work {
    entered: Notify,
    finished: Notify,
    gate: NativeGate,
    publication: Semaphore,
    mutations: AtomicUsize,
    token: Mutex<Option<Cancellation>>,
}
impl Work {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            finished: Notify::new(),
            gate: NativeGate::default(),
            publication: Semaphore::new(0),
            mutations: AtomicUsize::new(0),
            token: Mutex::new(None),
        })
    }
    fn token(&self) -> Cancellation {
        self.token.lock().unwrap().clone().unwrap()
    }
}

struct Handler(Arc<Work>);
impl CommandHandler for Handler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let work = self.0.clone();
        Box::pin(async move {
            let mode = params["mode"].as_str().unwrap_or("success").to_owned();
            let token = context.cancellation();
            *work.token.lock().unwrap() = Some(token.clone());
            let native = work.clone();
            let native_mode = mode.clone();
            let result = tokio::task::spawn_blocking(move || {
                let result = (|| {
                    if native_mode == "precommit" {
                        native.entered.notify_one();
                        native.gate.wait();
                    }
                    let _permit = token.begin_commit()?;
                    if native_mode != "precommit" {
                        native.entered.notify_one();
                        native.gate.wait();
                    }
                    if native_mode == "fail" {
                        return Err(ErrorCode::InvalidParams);
                    }
                    native.mutations.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"committed":true}))
                })();
                // The permit has left scope, but the seal must still protect us.
                native.finished.notify_one();
                result
            })
            .await
            .map_err(|_| ErrorCode::Internal)?;
            if mode == "publication" {
                work.publication.acquire().await.unwrap().forget();
            }
            if mode == "panic" {
                panic!("synthetic private commit panic");
            }
            if mode == "false_cancel" {
                return Err(ErrorCode::Cancelled);
            }
            if mode == "flood" {
                for _ in 0..QUEUE_CAPACITY * 3 {
                    context.emit("progress", json!({})).await?;
                }
            }
            result
        })
    }
}

struct WaitHandler;
impl CommandHandler for WaitHandler {
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        Box::pin(async move {
            context.emit("started", json!({})).await?;
            std::future::pending().await
        })
    }
}

#[derive(Default)]
struct TransportControl {
    fail: AtomicBool,
    pause_terminal: AtomicBool,
    observed: Notify,
    waker: AtomicWaker,
}

struct TestReader(ReadHalf<DuplexStream>, Arc<TransportControl>);
impl AsyncRead for TestReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.1.waker.register(cx.waker());
        if self.1.fail.load(Ordering::SeqCst) {
            self.1.observed.notify_one();
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        }
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

struct TestWriter(WriteHalf<DuplexStream>, Arc<TransportControl>);
impl AsyncWrite for TestWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.1.waker.register(cx.waker());
        if self.1.fail.load(Ordering::SeqCst) {
            self.1.observed.notify_one();
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if self.1.pause_terminal.load(Ordering::SeqCst) {
            let frame: Value = serde_json::from_slice(buf).unwrap();
            if frame["id"] == "job" && frame["type"] == "result" {
                self.1.observed.notify_one();
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

struct Client {
    writer: WriteHalf<DuplexStream>,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    task: tokio::task::JoinHandle<io::Result<()>>,
    input: Arc<TransportControl>,
    output: Arc<TransportControl>,
}
impl Client {
    fn new(work: Arc<Work>) -> Self {
        let mut registry = Registry::foundation();
        registry
            .install("chat", "chat", Arc::new(Handler(work.clone())))
            .unwrap();
        registry
            .install("index_papers_stream", "indexing", Arc::new(Handler(work)))
            .unwrap();
        registry
            .install("chat_stream", "chat", Arc::new(WaitHandler))
            .unwrap();
        let (client, server) = tokio::io::duplex(MAX_FRAME_BYTES * 2);
        let (read, write) = tokio::io::split(server);
        let input = Arc::new(TransportControl::default());
        let output = Arc::new(TransportControl::default());
        let task = tokio::spawn(serve(
            TestReader(read, input.clone()),
            TestWriter(write, output.clone()),
            registry,
        ));
        let (read, writer) = tokio::io::split(client);
        Self {
            writer,
            reader: BufReader::new(read).lines(),
            task,
            input,
            output,
        }
    }
    async fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.unwrap();
    }
    async fn request(&mut self, id: &str, command: &str, mode: &str) {
        let params = if command == "health" {
            json!({})
        } else {
            json!({"mode":mode})
        };
        self.send(json!({"v":1,"type":"request","id":id,"command":command,"params":params}))
            .await;
    }
    async fn cancel(&mut self, id: &str) {
        self.send(json!({"v":1,"type":"cancel","id":id})).await;
    }
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(DEADLINE, self.reader.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn finish(mut self) -> io::Result<()> {
        self.writer.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(DEADLINE, self.reader.next_line())
                .await
                .unwrap()
                .unwrap()
                .is_none(),
            "unexpected extra frame"
        );
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
    }
}

#[test]
fn cancellation_and_final_commit_have_exactly_one_atomic_winner() {
    for _ in 0..200 {
        let token = Cancellation::default();
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let commit_token = token.clone();
            let start = barrier.clone();
            let commit = scope.spawn(move || {
                start.wait();
                commit_token.begin_commit()
            });
            barrier.wait();
            let cancelled = token.cancel();
            let permit = commit.join().unwrap();
            assert_ne!(cancelled, permit.is_ok());
            if cancelled {
                assert_eq!(permit.unwrap_err(), ErrorCode::Cancelled);
                assert_eq!(token.check(), Err(ErrorCode::Cancelled));
                assert!(token.cancel());
            } else {
                assert!(token.check().is_ok());
                assert!(!token.cancel());
                assert_eq!(token.begin_commit().unwrap_err(), ErrorCode::Internal);
            }
        });
    }
}

#[tokio::test]
async fn cancel_before_commit_prevents_native_mutation_even_after_async_abort() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "chat", "precommit").await;
    notified(&work.entered).await;
    client.cancel("job").await;
    assert_eq!(client.next().await["type"], "cancelled");
    assert_eq!(client.next().await["accepted"], true);
    work.gate.release();
    notified(&work.finished).await;
    assert_eq!(work.mutations.load(Ordering::SeqCst), 0);
    assert_eq!(
        work.token().begin_commit().unwrap_err(),
        ErrorCode::Cancelled
    );
    client.finish().await.unwrap();
}

#[tokio::test]
async fn sealed_commit_rejects_cancel_without_blocking_health_or_other_cancellation() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "chat", "success").await;
    notified(&work.entered).await;
    client.cancel("job").await;
    assert_eq!(client.next().await["accepted"], false);
    client.request("health", "health", "").await;
    assert_eq!(client.next().await["data"]["ok"], true);
    client.request("read", "chat_stream", "").await;
    assert_eq!(client.next().await["type"], "event");
    client.cancel("read").await;
    assert_eq!(client.next().await["type"], "cancelled");
    assert_eq!(client.next().await["accepted"], true);
    work.gate.release();
    let result = client.next().await;
    assert_eq!(result["id"], "job");
    assert_eq!(result["type"], "result");
    assert_eq!(result["seq"], 0);
    assert_eq!(work.mutations.load(Ordering::SeqCst), 1);
    client.cancel("job").await;
    assert_eq!(client.next().await["accepted"], false);
    client.finish().await.unwrap();
}

#[tokio::test]
async fn dropped_permit_stays_sealed_until_handler_returns_and_result_is_published() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "chat", "publication").await;
    notified(&work.entered).await;
    work.gate.release();
    notified(&work.finished).await;
    assert!(!work.token().cancel());
    client.cancel("job").await;
    assert_eq!(client.next().await["accepted"], false);
    client.output.pause_terminal.store(true, Ordering::SeqCst);
    work.publication.add_permits(1);
    notified(&client.output.observed).await;
    assert!(
        !work.token().cancel(),
        "seal survives terminal write backpressure"
    );
    client.cancel("job").await;
    client.output.pause_terminal.store(false, Ordering::SeqCst);
    client.output.waker.wake();
    assert_eq!(client.next().await["type"], "result");
    assert_eq!(client.next().await["accepted"], false);
    client.finish().await.unwrap();
}

#[tokio::test]
async fn commit_error_panic_and_incorrect_cancel_remain_errors_not_cancellation() {
    for (mode, code, mutations) in [
        ("fail", "invalid_params", 0),
        ("panic", "internal", 1),
        ("false_cancel", "internal", 1),
    ] {
        let work = Work::new();
        let mut client = Client::new(work.clone());
        client.request("job", "chat", mode).await;
        notified(&work.entered).await;
        client.cancel("job").await;
        assert_eq!(client.next().await["accepted"], false);
        work.gate.release();
        let result = client.next().await;
        assert_eq!(result["type"], "error");
        assert_eq!(result["error"]["code"], code);
        assert!(!result.to_string().contains("synthetic private"));
        assert!(!work.token().cancel());
        assert_eq!(work.mutations.load(Ordering::SeqCst), mutations);
        client.request("alive", "health", "").await;
        assert_eq!(client.next().await["data"]["ok"], true);
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn eof_cancels_ordinary_work_first_and_drains_sealed_result() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "chat", "success").await;
    notified(&work.entered).await;
    client.request("read", "chat_stream", "").await;
    client.next().await;
    client.writer.shutdown().await.unwrap();
    let cancelled = client.next().await;
    assert_eq!(cancelled["id"], "read");
    assert_eq!(cancelled["type"], "cancelled");
    assert!(!client.task.is_finished());
    work.gate.release();
    assert_eq!(client.next().await["type"], "result");
    assert_eq!(work.mutations.load(Ordering::SeqCst), 1);
    client.finish().await.unwrap();
}

#[tokio::test]
async fn input_error_preserves_output_for_protected_result_and_returns_error() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "chat", "success").await;
    notified(&work.entered).await;
    client.request("read", "chat_stream", "").await;
    client.next().await;
    client.input.fail.store(true, Ordering::SeqCst);
    client.input.waker.wake();
    assert_eq!(client.next().await["type"], "cancelled");
    assert!(!client.task.is_finished());
    work.gate.release();
    assert_eq!(client.next().await["type"], "result");
    assert_eq!(
        client.finish().await.unwrap_err().kind(),
        io::ErrorKind::ConnectionReset
    );
}

#[tokio::test]
async fn broken_output_drains_sealed_work_and_full_progress_queue_before_returning_error() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "index_papers_stream", "flood").await;
    notified(&work.entered).await;
    client.output.fail.store(true, Ordering::SeqCst);
    client.request("probe", "health", "").await;
    notified(&client.output.observed).await;
    assert!(!client.task.is_finished());
    assert!(!work.token().cancel());
    work.gate.release();
    let error = tokio::time::timeout(DEADLINE, client.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(work.mutations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn every_shutdown_path_prevents_unsealed_native_commit_after_dispatcher_exit() {
    for failure in [
        None,
        Some(io::ErrorKind::ConnectionReset),
        Some(io::ErrorKind::BrokenPipe),
    ] {
        let work = Work::new();
        let mut client = Client::new(work.clone());
        client.request("job", "chat", "precommit").await;
        notified(&work.entered).await;
        match failure {
            None => client.writer.shutdown().await.unwrap(),
            Some(io::ErrorKind::ConnectionReset) => {
                client.input.fail.store(true, Ordering::SeqCst);
                client.input.waker.wake();
            }
            Some(_) => {
                client.output.fail.store(true, Ordering::SeqCst);
                client.request("probe", "health", "").await;
            }
        }
        if failure != Some(io::ErrorKind::BrokenPipe) {
            assert_eq!(client.next().await["type"], "cancelled");
        }
        let result = tokio::time::timeout(DEADLINE, client.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.err().map(|e| e.kind()), failure);
        assert_eq!(work.token().check(), Err(ErrorCode::Cancelled));
        work.gate.release();
        notified(&work.finished).await;
        assert_eq!(work.mutations.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn lost_final_result_is_transport_error_not_rollback_or_mutation_replay() {
    let work = Work::new();
    let mut client = Client::new(work.clone());
    client.request("job", "chat", "publication").await;
    notified(&work.entered).await;
    work.gate.release();
    notified(&work.finished).await;
    client.output.fail.store(true, Ordering::SeqCst);
    work.publication.add_permits(1);
    let error = tokio::time::timeout(DEADLINE, client.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(work.mutations.load(Ordering::SeqCst), 1);
    assert!(!work.token().cancel());
}

#[test]
fn real_sqlite_commit_failure_does_not_unseal_or_claim_success() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("commit-failure.sqlite");
    let sql = sqlite::open(&path).unwrap();
    sql.execute("PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);").unwrap();
    let token = Cancellation::default();
    sql.execute("BEGIN; INSERT INTO child VALUES(42);").unwrap();
    {
        let _permit = token.begin_commit().unwrap();
        assert!(
            sql.execute("COMMIT").is_err(),
            "deferred constraint fails at COMMIT"
        );
        sql.execute("ROLLBACK").unwrap();
    }
    assert!(!token.cancel());
    assert_eq!(token.check(), Ok(()));
    let reopened = sqlite::open(&path).unwrap();
    let mut count = reopened.prepare("SELECT COUNT(*) FROM child").unwrap();
    assert_eq!(count.next().unwrap(), sqlite::State::Row);
    assert_eq!(count.read::<i64, _>(0).unwrap(), 0);
}
