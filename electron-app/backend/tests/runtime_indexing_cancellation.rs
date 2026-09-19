//! Runtime prerequisite only: fake supervisors and temporary native SQLite/store
//! workers. No production indexing, durable recovery claims, providers or user data.
use futures_util::{future::join_all, task::AtomicWaker};
use research_backend::{
    protocol::{ErrorCode, MAX_ACTIVE_REQUESTS, MAX_FRAME_BYTES, QUEUE_CAPACITY},
    registry::{BackendFuture, CommandHandler, Registry},
    retrieval::{EmbeddingIdentity, LexicalStatus, PreparedOperation, VectorStore},
    runtime::{Cancellation, CancellationMode, OperationGroup, RequestContext, ResetStep, serve},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    future::Future,
    io,
    path::PathBuf,
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
    sync::{Notify, Semaphore, oneshot},
};

const DEADLINE: Duration = Duration::from_secs(10);
// Runtime deliberately sanitizes worker panics. Keep barrier failures visible to
// the test driver so an expected Internal terminal cannot hide a gate timeout.
static NATIVE_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    let result = tokio::time::timeout(DEADLINE, future)
        .await
        .expect("test phase timed out");
    assert_eq!(
        NATIVE_TIMEOUTS.load(Ordering::SeqCst),
        0,
        "native barrier timed out"
    );
    result
}

#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);
impl Gate {
    fn wait(&self) {
        let (open, _) = self
            .1
            .wait_timeout_while(self.0.lock().unwrap(), DEADLINE, |open| !*open)
            .unwrap();
        if !*open {
            NATIVE_TIMEOUTS.fetch_add(1, Ordering::SeqCst);
            panic!("native gate timed out");
        }
    }
    fn release(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
}

struct Exit(Arc<AtomicBool>);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
thread_local! { static EXITS: RefCell<Vec<Exit>> = const { RefCell::new(Vec::new()) }; }

#[derive(Default)]
struct Step {
    entered: Notify,
    gate: Gate,
    exited: Arc<AtomicBool>,
    token: Mutex<Option<Cancellation>>,
}
impl Step {
    fn enter(&self, token: &Cancellation) {
        EXITS.with(|exits| exits.borrow_mut().push(Exit(self.exited.clone())));
        *self.token.lock().unwrap() = Some(token.clone());
        self.entered.notify_one();
        self.gate.wait();
    }
    fn token(&self) -> Cancellation {
        self.token.lock().unwrap().clone().unwrap()
    }
}

fn database() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("work.sqlite");
    sqlite::open(&path)
        .unwrap()
        .execute("CREATE TABLE records(id INTEGER PRIMARY KEY)")
        .unwrap();
    (root, path)
}
fn rows(path: &PathBuf) -> Vec<i64> {
    let sql = sqlite::open(path).unwrap();
    let mut query = sql.prepare("SELECT id FROM records ORDER BY id").unwrap();
    let mut result = vec![];
    while query.next().unwrap() == sqlite::State::Row {
        result.push(query.read(0).unwrap());
    }
    result
}
fn sql_mutation(
    path: PathBuf,
    step: Arc<Step>,
    after_seal: bool,
    id: i64,
) -> impl FnOnce(Cancellation) -> BackendFuture<()> + Send + 'static {
    move |token| {
        Box::pin(async move {
            let sql = sqlite::open(path).map_err(|_| ErrorCode::Internal)?;
            sql.execute(format!("BEGIN IMMEDIATE; INSERT INTO records VALUES({id})"))
                .map_err(|_| ErrorCode::Internal)?;
            if after_seal {
                let _permit = token.begin_commit()?;
            }
            step.enter(&token);
            if !after_seal {
                let _permit = token.begin_commit()?;
            }
            sql.execute("COMMIT").map_err(|_| ErrorCode::Internal)?;
            Ok(())
        })
    }
}

struct Handler<F>(F);
impl<F> CommandHandler for Handler<F>
where
    F: Fn(Value, RequestContext) -> BackendFuture<Value> + Send + Sync,
{
    fn cancellation_mode(&self) -> CancellationMode {
        CancellationMode::Cooperative
    }
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        (self.0)(params, context)
    }
}

#[derive(Default)]
struct Control {
    fail: AtomicBool,
    pause: AtomicBool,
    observed: Notify,
    waker: AtomicWaker,
}
struct Reader(ReadHalf<DuplexStream>, Arc<Control>);
impl AsyncRead for Reader {
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
struct Writer(WriteHalf<DuplexStream>, Arc<Control>);
impl AsyncWrite for Writer {
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
        if self.1.pause.load(Ordering::SeqCst) {
            self.1.observed.notify_one();
            return Poll::Pending;
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
    input: Arc<Control>,
    output: Arc<Control>,
}
impl Client {
    fn new(
        handler: impl Fn(Value, RequestContext) -> BackendFuture<Value> + Send + Sync + 'static,
    ) -> Self {
        let mut registry = Registry::foundation();
        registry
            .install(
                "index_papers_stream",
                "indexing",
                Arc::new(Handler(handler)),
            )
            .unwrap();
        Self::registry(registry)
    }
    fn registry(registry: Registry) -> Self {
        let (client, server) = tokio::io::duplex(MAX_FRAME_BYTES * 2);
        let (read, write) = tokio::io::split(server);
        let input = Arc::new(Control::default());
        let output = Arc::new(Control::default());
        let task = tokio::spawn(serve(
            Reader(read, input.clone()),
            Writer(write, output.clone()),
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
        self.writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn request(&mut self, id: &str) {
        self.send(
            json!({"v":1,"type":"request","id":id,"command":"index_papers_stream","params":{}}),
        )
        .await;
    }
    async fn health(&mut self) {
        self.send(json!({"v":1,"type":"request","id":"health","command":"health","params":{}}))
            .await;
        let frame = self.next().await;
        assert_eq!(
            frame["id"], "health",
            "no early terminal/progress while held"
        );
        assert_eq!(frame["data"]["ok"], true);
    }
    async fn cancel(&mut self, id: &str, accepted: bool) {
        self.send(json!({"v":1,"type":"cancel","id":id})).await;
        let frame = self.next().await;
        assert_eq!(frame["type"], "cancel_ack");
        assert_eq!(frame["accepted"], accepted);
    }
    async fn next(&mut self) -> Value {
        serde_json::from_str(&bounded(self.reader.next_line()).await.unwrap().unwrap()).unwrap()
    }
    async fn finish(mut self) -> io::Result<()> {
        self.writer.shutdown().await.unwrap();
        assert!(
            bounded(self.reader.next_line()).await.unwrap().is_none(),
            "extra terminal/event"
        );
        bounded(self.task).await.unwrap()
    }
}

#[tokio::test]
async fn native_child_preseal_stop_rolls_back_but_postseal_stop_waits_for_real_commit() {
    for after_seal in [false, true] {
        let (_root, path) = database();
        let step = Arc::new(Step::default());
        let native = step.clone();
        let db = path.clone();
        let mut client = Client::new(move |_, context| {
            let step = native.clone();
            let path = db.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                let result = group
                    .run_mutation("sqlite", sql_mutation(path, step.clone(), after_seal, 1))
                    .await;
                assert!(step.exited.load(Ordering::SeqCst));
                if after_seal {
                    result?;
                } else {
                    assert_eq!(result, Err(ErrorCode::Cancelled));
                }
                assert_eq!(
                    group
                        .run_mutation("forbidden", |_| async { panic!("admitted after stop") })
                        .await,
                    Err::<(), _>(ErrorCode::Cancelled)
                );
                Err(ErrorCode::Cancelled)
            })
        });
        client.request("job").await;
        bounded(step.entered.notified()).await;
        client.cancel("job", true).await;
        client.cancel("job", true).await;
        assert_eq!(step.token().check().is_ok(), after_seal);
        client.health().await;
        assert!(!step.exited.load(Ordering::SeqCst));
        step.gate.release();
        let terminal = client.next().await;
        assert_eq!(terminal["type"], "cancelled");
        assert_eq!(terminal["seq"], 0);
        assert!(step.exited.load(Ordering::SeqCst));
        assert_eq!(rows(&path), if after_seal { vec![1] } else { vec![] });
        client.cancel("job", false).await;
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn separate_seals_preserve_prior_commit_and_join_local_compensation() {
    for cleanup_fails in [false, true] {
        let (_root, path) = database();
        let first = Arc::new(Step::default());
        first.gate.release();
        let second = Arc::new(Step::default());
        let cleanup = Arc::new(Step::default());
        let a = first.clone();
        let b = second.clone();
        let c = cleanup.clone();
        let db = path.clone();
        let mut client = Client::new(move |_, context| {
            let a = a.clone();
            let b = b.clone();
            let c = c.clone();
            let db = db.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                group
                    .run_mutation("paper-one", sql_mutation(db.clone(), a, false, 1))
                    .await?;
                assert!(group.check().is_ok(), "first seal must not seal job");
                group
                    .run_mutation("paper-two", sql_mutation(db.clone(), b, true, 2))
                    .await?;
                assert_eq!(
                    context.emit("late", json!({})).await,
                    Err(ErrorCode::Cancelled)
                );
                group
                    .run_compensation("restore-unpublished-paper", move |token| async move {
                        assert!(!token.cancel(), "compensation cannot be re-cancelled");
                        c.enter(&token);
                        if cleanup_fails {
                            return Err(ErrorCode::InvalidParams);
                        }
                        let sql = sqlite::open(db).unwrap();
                        sql.execute("BEGIN; DELETE FROM records WHERE id=2")
                            .unwrap();
                        let _permit = token.begin_commit()?;
                        sql.execute("COMMIT").unwrap();
                        Ok(())
                    })
                    .await?;
                Err(ErrorCode::Cancelled)
            })
        });
        client.request("job").await;
        bounded(second.entered.notified()).await;
        assert!(first.exited.load(Ordering::SeqCst));
        client.cancel("job", true).await;
        second.gate.release();
        bounded(cleanup.entered.notified()).await;
        assert_eq!(rows(&path), vec![1, 2]);
        client.cancel("job", true).await;
        client.health().await;
        cleanup.gate.release();
        let terminal = client.next().await;
        assert_eq!(
            terminal["type"],
            if cleanup_fails { "error" } else { "cancelled" }
        );
        if cleanup_fails {
            assert_eq!(terminal["error"]["code"], "invalid_params");
        }
        assert!(cleanup.exited.load(Ordering::SeqCst));
        assert_eq!(
            rows(&path),
            if cleanup_fails { vec![1, 2] } else { vec![1] }
        );
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn real_vector_store_sequence_uses_distinct_children_then_final_completion() {
    let root = tempfile::tempdir().unwrap();
    let store = VectorStore::new(
        root.path(),
        &root.path().join("vectors.sqlite"),
        EmbeddingIdentity {
            model: "fake".into(),
            dimension: 1,
        },
    )
    .unwrap();
    store.initialize(&Cancellation::default()).unwrap();
    let native = store.clone();
    let mut client = Client::new(move |_, context| {
        let store = native.clone();
        Box::pin(async move {
            let group = context.operation_group()?;
            let mut tokens = vec![];
            for revision in 0..3 {
                let store = store.clone();
                let token = group
                    .run_mutation("vector-commit", move |token| async move {
                        let status = store
                            .apply(
                                &[
                                    PreparedOperation::DeletePaper("absent".into()),
                                    PreparedOperation::RebuildLexical,
                                ],
                                Some(revision),
                                &token,
                            )
                            .map_err(|_| ErrorCode::Internal)?;
                        assert_eq!(status.revision, revision + 1);
                        assert_eq!(status.lexical, LexicalStatus::Ready);
                        Ok(token)
                    })
                    .await?;
                assert!(!token.cancel());
                tokens.push(token);
                assert!(group.check().is_ok());
            }
            let _permit = group.seal_completion()?;
            assert!(!group.preparation_stop().cancel());
            assert!(group.seal_completion().is_err());
            for token in tokens {
                assert!(token.begin_commit().is_err());
            }
            Ok(json!({"commits":3}))
        })
    });
    client.request("job").await;
    assert_eq!(client.next().await["data"]["commits"], 3);
    assert_eq!(store.status(&Cancellation::default()).unwrap().revision, 3);
    client.finish().await.unwrap();
}

#[tokio::test]
async fn capacity_is_retained_for_all_cooperative_native_drains() {
    let steps: Vec<_> = (0..MAX_ACTIVE_REQUESTS)
        .map(|_| Arc::new(Step::default()))
        .collect();
    let natives = steps.clone();
    let index = AtomicUsize::new(0);
    let mut client = Client::new(move |_, context| {
        let step = natives[index.fetch_add(1, Ordering::SeqCst)].clone();
        Box::pin(async move {
            context
                .operation_group()?
                .run_preparation("native-read", move |token| async move {
                    step.enter(&token);
                    token.check()
                })
                .await?;
            unreachable!()
        })
    });
    for (i, step) in steps.iter().enumerate() {
        client.request(&format!("job{i}")).await;
        bounded(step.entered.notified()).await;
        client.cancel(&format!("job{i}"), true).await;
    }
    client.request("overflow").await;
    assert_eq!(client.next().await["error"]["code"], "busy");
    client.cancel("job0", true).await;
    client.health().await;
    for step in &steps {
        assert!(!step.exited.load(Ordering::SeqCst));
        step.gate.release();
    }
    let mut ids = std::collections::HashSet::new();
    for _ in &steps {
        let frame = client.next().await;
        assert_eq!(frame["type"], "cancelled");
        let id = frame["id"].as_str().unwrap();
        assert!(ids.insert(id.to_owned()));
        let index: usize = id.strip_prefix("job").unwrap().parse().unwrap();
        assert!(steps[index].exited.load(Ordering::SeqCst));
    }
    client.finish().await.unwrap();
}

#[tokio::test]
async fn stop_before_admission_and_invalid_transitions_do_not_execute_closures() {
    let mut client = Client::new(|_, context| {
        Box::pin(async move {
            let group = context.operation_group()?;
            assert_eq!(
                context.cancellation().begin_commit().unwrap_err(),
                ErrorCode::Internal
            );
            assert_eq!(
                group
                    .run_compensation("early", |_| async { panic!("early cleanup admitted") })
                    .await,
                Err::<(), _>(ErrorCode::Internal)
            );
            assert!(context.cancellation().cancel());
            assert_eq!(group.check(), Err(ErrorCode::Cancelled));
            assert_eq!(
                group
                    .run_mutation("late", |_| async { panic!("late child") })
                    .await,
                Err::<(), _>(ErrorCode::Cancelled)
            );
            assert_eq!(
                group
                    .run_preparation("late", |_| async { panic!("late prep") })
                    .await,
                Err::<(), _>(ErrorCode::Cancelled)
            );
            assert_eq!(group.seal_completion().unwrap_err(), ErrorCode::Cancelled);
            assert!(group.begin_irreversible_plan("op".into()).is_err());
            Err(ErrorCode::Cancelled)
        })
    });
    client.request("job").await;
    assert_eq!(client.next().await["type"], "cancelled");
    client.finish().await.unwrap();
}

#[tokio::test]
async fn overlap_and_completion_with_active_child_are_rejected() {
    let step = Arc::new(Step::default());
    let native = step.clone();
    let mut client = Client::new(move |_, context| {
        let step = native.clone();
        Box::pin(async move {
            let group = context.operation_group()?;
            let entered = step.clone();
            let mut future = Box::pin(group.run_mutation("first", move |token| async move {
                step.enter(&token);
                let _permit = token.begin_commit()?;
                Ok(())
            }));
            assert!(futures_util::poll!(&mut future).is_pending());
            entered.entered.notified().await;
            assert_eq!(
                group
                    .run_mutation("overlap", |_| async { panic!("overlap") })
                    .await,
                Err::<(), _>(ErrorCode::Internal)
            );
            assert!(group.seal_completion().is_err());
            assert!(group.begin_irreversible_plan("reset".into()).is_err());
            entered.gate.release();
            future.await?;
            let _permit = group.seal_completion()?;
            Ok(json!({}))
        })
    });
    client.request("job").await;
    assert_eq!(client.next().await["type"], "result");
    assert!(step.exited.load(Ordering::SeqCst));
    client.finish().await.unwrap();
}

#[tokio::test]
async fn cancellation_never_masks_worker_panic_handler_panic_or_swallowed_cleanup_error() {
    for mode in [
        "worker",
        "handler",
        "cleanup",
        "false-success",
        "false-cancel",
        "sealed-cancel",
    ] {
        let step = Arc::new(Step::default());
        let native = step.clone();
        let mut client = Client::new(move |_, context| {
            let step = native.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                let result = group
                    .run_mutation("failure", move |token| async move {
                        if mode == "sealed-cancel" {
                            let _permit = token.begin_commit()?;
                        }
                        step.enter(&token);
                        if mode == "worker" {
                            panic!("private worker detail");
                        }
                        if mode == "sealed-cancel" {
                            Err(ErrorCode::Cancelled)
                        } else {
                            token.check()
                        }
                    })
                    .await;
                if mode == "handler" {
                    panic!("private handler detail");
                }
                if mode == "cleanup" {
                    assert_eq!(
                        group
                            .run_compensation("fails", |_| async {
                                Err::<(), _>(ErrorCode::InvalidParams)
                            })
                            .await,
                        Err(ErrorCode::InvalidParams)
                    );
                }
                if mode == "false-success" {
                    return Ok(json!({}));
                }
                if mode == "false-cancel" {
                    return Err(ErrorCode::Cancelled);
                }
                let _ = result;
                Err(ErrorCode::Cancelled)
            })
        });
        client.request("job").await;
        bounded(step.entered.notified()).await;
        client.cancel("job", true).await;
        assert_eq!(step.token().check().is_ok(), mode == "sealed-cancel");
        step.gate.release();
        let frame = client.next().await;
        assert_eq!(
            frame["type"],
            if mode == "false-cancel" {
                "cancelled"
            } else {
                "error"
            }
        );
        if mode != "false-cancel" {
            assert_eq!(
                frame["error"]["code"],
                if mode == "cleanup" {
                    "invalid_params"
                } else {
                    "internal"
                }
            );
        }
        assert!(!frame.to_string().contains("private"));
        assert!(step.exited.load(Ordering::SeqCst));
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn dropped_aborted_and_forgotten_helpers_retain_native_worker_until_exit() {
    for mode in ["drop", "abort", "forget", "panic"] {
        let (_root, path) = database();
        let step = Arc::new(Step::default());
        let native = step.clone();
        let db = path.clone();
        let ready = Arc::new(Notify::new());
        let signal = ready.clone();
        let repaired = Arc::new(AtomicBool::new(false));
        let repair_done = repaired.clone();
        let mut client = Client::new(move |_, context| {
            let step = native.clone();
            let path = db.clone();
            let signal = signal.clone();
            let repair_done = repair_done.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                let mut future = Box::pin(
                    group.run_mutation("owned-sqlite", sql_mutation(path, step.clone(), true, 1)),
                );
                assert!(futures_util::poll!(&mut future).is_pending());
                step.entered.notified().await;
                signal.notify_one();
                match mode {
                    "drop" => drop(future),
                    "forget" => std::mem::forget(future),
                    "panic" => panic!("handler abandons admitted child"),
                    "abort" => {
                        let (abort, registration) = futures_util::future::AbortHandle::new_pair();
                        abort.abort();
                        assert!(
                            futures_util::future::Abortable::new(future, registration)
                                .await
                                .is_err()
                        );
                    }
                    _ => unreachable!(),
                }
                if matches!(mode, "drop" | "abort") {
                    assert!(step.exited.load(Ordering::SeqCst));
                    assert_eq!(group.check(), Err(ErrorCode::Internal));
                    group
                        .run_compensation("resolved-local-repair", |token| async move {
                            let _permit = token.begin_commit()?;
                            Ok(())
                        })
                        .await?;
                    repair_done.store(true, Ordering::SeqCst);
                }
                Err(ErrorCode::Cancelled)
            })
        });
        client.request("job").await;
        bounded(ready.notified()).await;
        client.cancel("job", true).await;
        client.health().await;
        assert!(!step.exited.load(Ordering::SeqCst));
        step.gate.release();
        let frame = client.next().await;
        assert_eq!(frame["error"]["code"], "internal");
        assert!(step.exited.load(Ordering::SeqCst));
        assert_eq!(rows(&path), vec![1]);
        assert_eq!(
            repaired.load(Ordering::SeqCst),
            matches!(mode, "drop" | "abort")
        );
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn eof_read_error_output_loss_and_stall_drain_child_and_compensation() {
    for mode in ["eof", "read", "output", "stall"] {
        let (_root, path) = database();
        let step = Arc::new(Step::default());
        let native = step.clone();
        let db = path.clone();
        let cleanup = Arc::new(Step::default());
        let repair = cleanup.clone();
        let token = Arc::new(Mutex::new(None::<Cancellation>));
        let capture = token.clone();
        let mut client = Client::new(move |_, context| {
            let step = native.clone();
            let db = db.clone();
            let repair = repair.clone();
            let capture = capture.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                *capture.lock().unwrap() = Some(group.preparation_stop());
                group
                    .run_mutation("sealed", sql_mutation(db.clone(), step, true, 1))
                    .await?;
                assert_eq!(group.check(), Err(ErrorCode::Cancelled));
                group
                    .run_compensation("delete-unpublished", move |token| async move {
                        repair.enter(&token);
                        let sql = sqlite::open(db).unwrap();
                        sql.execute("BEGIN; DELETE FROM records").unwrap();
                        let _permit = token.begin_commit()?;
                        sql.execute("COMMIT").unwrap();
                        Ok(())
                    })
                    .await?;
                Err(ErrorCode::Cancelled)
            })
        });
        client.request("job").await;
        bounded(step.entered.notified()).await;
        match mode {
            "eof" => client.writer.shutdown().await.unwrap(),
            "read" => {
                client.input.fail.store(true, Ordering::SeqCst);
                client.input.waker.wake();
            }
            "output" | "stall" => {
                if mode == "output" {
                    client.output.fail.store(true, Ordering::SeqCst);
                } else {
                    client.output.pause.store(true, Ordering::SeqCst);
                }
                client
                    .send(
                        json!({"v":1,"type":"request","id":"probe","command":"health","params":{}}),
                    )
                    .await;
            }
            _ => unreachable!(),
        }
        let stop = token.lock().unwrap().clone().unwrap();
        bounded(async {
            while stop.check().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(!client.task.is_finished());
        assert!(!step.exited.load(Ordering::SeqCst));
        step.gate.release();
        bounded(cleanup.entered.notified()).await;
        assert!(step.exited.load(Ordering::SeqCst));
        assert!(!client.task.is_finished());
        cleanup.gate.release();
        if matches!(mode, "eof" | "read") {
            assert_eq!(client.next().await["type"], "cancelled");
        }
        let result = bounded(client.task).await.unwrap();
        assert_eq!(
            result.err().map(|e| e.kind()),
            match mode {
                "eof" => None,
                "read" => Some(io::ErrorKind::ConnectionReset),
                "output" => Some(io::ErrorKind::BrokenPipe),
                _ => Some(io::ErrorKind::TimedOut),
            }
        );
        assert!(cleanup.exited.load(Ordering::SeqCst));
        assert!(rows(&path).is_empty());
    }
}

#[tokio::test]
async fn queued_progress_is_suppressed_after_stop_and_full_queue_drains_on_output_loss() {
    for sealed in [false, true] {
        let ready = Arc::new(Notify::new());
        let signal = ready.clone();
        let release = Arc::new(Semaphore::new(0));
        let gate = release.clone();
        let captured = Arc::new(Mutex::new(None::<Cancellation>));
        let capture = captured.clone();
        let mut client = Client::new(move |_, context| {
            let signal = signal.clone();
            let gate = gate.clone();
            let capture = capture.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                *capture.lock().unwrap() = Some(group.preparation_stop());
                if sealed {
                    let _permit = group.seal_completion()?;
                }
                signal.notify_one();
                gate.acquire().await.unwrap().forget();
                for _ in 0..QUEUE_CAPACITY * 3 {
                    context.emit("progress", json!({})).await?;
                }
                Ok(json!({"done":true}))
            })
        });
        client.request("job").await;
        bounded(ready.notified()).await;
        client.output.pause.store(true, Ordering::SeqCst);
        release.add_permits(1);
        bounded(client.output.observed.notified()).await;
        let stop = captured.lock().unwrap().clone().unwrap();
        assert_eq!(stop.cancel(), !sealed);
        if sealed {
            client.output.fail.store(true, Ordering::SeqCst);
            client.output.waker.wake();
            assert_eq!(
                bounded(client.task).await.unwrap().unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            assert!(
                !stop.cancel(),
                "completion seal persists across broken output"
            );
        } else {
            // One event was selected BEFORE stop and is already in a transport
            // write. Remaining queued progress must be discarded, not published.
            client.output.pause.store(false, Ordering::SeqCst);
            client.output.waker.wake();
            assert_eq!(client.next().await["type"], "event");
            let frame = client.next().await;
            assert_eq!(frame["type"], "cancelled");
            assert_eq!(frame["seq"], 1);
            client.finish().await.unwrap();
        }
    }
}

const RESET_STEPS: [ResetStep; 6] = [
    ResetStep::PersistFinalizing,
    ResetStep::ResetRetrieval,
    ResetStep::RecordRetrieval,
    ResetStep::ResetGraph,
    ResetStep::RecordGraph,
    ResetStep::CompleteJournal,
];

#[tokio::test]
async fn reset_capability_is_ordered_restricted_sticky_and_stops_after_marker_failure() {
    for marker_fails in [false, true] {
        let ready = Arc::new(Notify::new());
        let signal = ready.clone();
        let release = Arc::new(Semaphore::new(0));
        let gate = release.clone();
        let checked = Arc::new(AtomicBool::new(false));
        let passed = checked.clone();
        let mut client = Client::new(move |_, context| {
            let signal = signal.clone();
            let gate = gate.clone();
            let passed = passed.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                let plan = group.begin_irreversible_plan("test-durable-intent".into())?;
                assert_eq!(plan.reset_op_id(), "test-durable-intent");
                assert!(group.begin_irreversible_plan("second".into()).is_err());
                assert!(group.seal_completion().is_err());
                assert!(plan.seal_completion().is_err());
                assert_eq!(
                    group
                        .run_mutation("ordinary", |_| async {
                            panic!("ordinary mutation in reset")
                        })
                        .await,
                    Err::<(), _>(ErrorCode::Internal)
                );
                assert_eq!(
                    group
                        .run_preparation("provider", |_| async { panic!("preparation in reset") })
                        .await,
                    Err::<(), _>(ErrorCode::Internal)
                );
                assert_eq!(
                    group
                        .run_compensation("cleanup", |_| async { panic!("compensation in reset") })
                        .await,
                    Err::<(), _>(ErrorCode::Internal)
                );
                assert_eq!(
                    plan.run_step(ResetStep::ResetRetrieval, |_| async {
                        panic!("marker skipped")
                    })
                    .await,
                    Err::<(), _>(ErrorCode::Internal)
                );
                signal.notify_one();
                gate.acquire().await.unwrap().forget();
                for step in RESET_STEPS {
                    let result = plan
                        .run_step(step, move |token| async move {
                            assert!(!token.cancel());
                            let _permit = token.begin_commit()?;
                            if marker_fails {
                                Err(ErrorCode::InvalidParams)
                            } else {
                                Ok(())
                            }
                        })
                        .await;
                    if marker_fails {
                        assert_eq!(result, Err(ErrorCode::InvalidParams));
                        assert_eq!(
                            plan.run_step(ResetStep::ResetRetrieval, |_| async {
                                panic!("destructive reset after failed marker")
                            })
                            .await,
                            Err::<(), _>(ErrorCode::Internal)
                        );
                        assert!(!group.preparation_stop().cancel());
                        passed.store(true, Ordering::SeqCst);
                        return Err(ErrorCode::Cancelled); // Must become the retained marker error.
                    }
                    result?;
                    assert_eq!(
                        plan.run_step(step, |_| async { panic!("duplicate reset step") })
                            .await,
                        Err::<(), _>(ErrorCode::Internal)
                    );
                }
                let _permit = plan.seal_completion()?;
                assert!(plan.seal_completion().is_err());
                Ok(json!({"finalized":true}))
            })
        });
        client.request("job").await;
        bounded(ready.notified()).await;
        client.cancel("job", false).await;
        client.health().await;
        release.add_permits(1);
        let frame = client.next().await;
        assert_eq!(frame["type"], if marker_fails { "error" } else { "result" });
        if marker_fails {
            assert_eq!(frame["error"]["code"], "invalid_params");
            assert!(checked.load(Ordering::SeqCst));
        }
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn completion_and_reset_transition_have_atomic_stop_winners() {
    for reset in [false, true] {
        for _ in 0..50 {
            let mut client = Client::new(move |_, context| {
                Box::pin(async move {
                    let group = context.operation_group()?;
                    let barrier = Arc::new(Barrier::new(2));
                    let start = barrier.clone();
                    let stop = group.preparation_stop();
                    let cancel = std::thread::spawn(move || {
                        start.wait();
                        stop.cancel()
                    });
                    barrier.wait();
                    let sealed = if reset {
                        match group.begin_irreversible_plan("intent".into()) {
                            Ok(plan) => {
                                for step in RESET_STEPS {
                                    plan.run_step(step, |token| async move {
                                        let _permit = token.begin_commit()?;
                                        Ok(())
                                    })
                                    .await?;
                                }
                                plan.seal_completion().map(|_| ())
                            }
                            Err(code) => Err(code),
                        }
                    } else {
                        group.seal_completion().map(|_| ())
                    };
                    assert_ne!(cancel.join().unwrap(), sealed.is_ok());
                    sealed?;
                    Ok(json!({}))
                })
            });
            client.request("job").await;
            let frame = client.next().await;
            assert!(matches!(
                frame["type"].as_str(),
                Some("result" | "cancelled")
            ));
            client.finish().await.unwrap();
        }
    }
}

#[tokio::test]
async fn preparation_bound_and_outliving_clones_cannot_admit_after_finish() {
    let (send, receive) = oneshot::channel::<OperationGroup>();
    let send = Mutex::new(Some(send));
    let mut client = Client::new(move |_, context| {
        let send = send.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let group = context.operation_group()?;
            let steps: Vec<_> = (0..32).map(|_| Arc::new(Step::default())).collect();
            let mut futures = vec![];
            for step in &steps {
                let native = step.clone();
                let mut future =
                    Box::pin(group.run_preparation("bounded", move |token| async move {
                        native.enter(&token);
                        token.check()
                    }));
                assert!(futures_util::poll!(&mut future).is_pending());
                step.entered.notified().await;
                futures.push(future);
            }
            assert_eq!(
                group
                    .run_preparation("overflow", |_| async { panic!("preparation overflow") })
                    .await,
                Err::<(), _>(ErrorCode::Busy)
            );
            assert!(group.seal_completion().is_err());
            for step in &steps {
                step.gate.release();
            }
            for result in join_all(futures).await {
                result?;
            }
            let leaked_token = group
                .run_mutation("no-op", |token| async { Ok(token) })
                .await?;
            assert!(
                leaked_token.begin_commit().is_err(),
                "unsealed completed child is revoked"
            );
            let _permit = group.seal_completion()?;
            assert!(send.send(group).is_ok());
            Ok(json!({}))
        })
    });
    client.request("job").await;
    let group = bounded(receive).await.unwrap();
    assert_eq!(client.next().await["type"], "result");
    assert_eq!(
        group
            .run_mutation("after-terminal", |_| async { panic!("late cloned group") })
            .await,
        Err::<(), _>(ErrorCode::Internal)
    );
    assert!(group.seal_completion().is_err());
    client.finish().await.unwrap();
}

#[tokio::test]
async fn default_immediate_handler_has_no_group_and_preserves_final_seal() {
    struct Immediate;
    impl CommandHandler for Immediate {
        fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
            Box::pin(async move {
                assert!(context.operation_group().is_err());
                let token = context.cancellation();
                let _permit = token.begin_commit()?;
                assert!(!token.cancel());
                Ok(json!({}))
            })
        }
    }
    assert_eq!(Immediate.cancellation_mode(), CancellationMode::Immediate);
    let mut registry = Registry::foundation();
    registry
        .install("index_papers_stream", "indexing", Arc::new(Immediate))
        .unwrap();
    let mut client = Client::registry(registry);
    client.request("job").await;
    assert_eq!(client.next().await["type"], "result");
    client.finish().await.unwrap();
}

#[tokio::test]
async fn concurrent_child_seal_and_stop_has_one_winner_without_sealing_the_parent() {
    for _ in 0..50 {
        let (_root, path) = database();
        let db = path.clone();
        let committed = Arc::new(AtomicBool::new(false));
        let capture = committed.clone();
        let mut client = Client::new(move |_, context| {
            let path = db.clone();
            let capture = capture.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                let barrier = Arc::new(Barrier::new(2));
                let start = barrier.clone();
                let stop = group.preparation_stop();
                let cancellation = std::thread::spawn(move || {
                    start.wait();
                    assert!(stop.cancel());
                });
                let result = group
                    .run_mutation("racing-commit", move |token| async move {
                        let sql = sqlite::open(path).unwrap();
                        sql.execute("BEGIN; INSERT INTO records VALUES(1)").unwrap();
                        barrier.wait();
                        let _permit = token.begin_commit()?;
                        sql.execute("COMMIT").unwrap();
                        capture.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .await;
                cancellation.join().unwrap();
                assert!(result.is_ok() || result == Err(ErrorCode::Cancelled));
                assert_eq!(group.check(), Err(ErrorCode::Cancelled));
                Err(ErrorCode::Cancelled)
            })
        });
        client.request("job").await;
        assert_eq!(client.next().await["type"], "cancelled");
        assert_eq!(
            rows(&path).len(),
            usize::from(committed.load(Ordering::SeqCst))
        );
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn real_sqlite_commit_failure_after_stop_remains_error_and_worker_is_joined() {
    let (_root, path) = database();
    sqlite::open(&path).unwrap().execute("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
    let step = Arc::new(Step::default());
    let native = step.clone();
    let db = path.clone();
    let mut client = Client::new(move |_, context| {
        let path = db.clone();
        let step = native.clone();
        Box::pin(async move {
            let group = context.operation_group()?;
            group
                .run_mutation("failing-commit", move |token| async move {
                    let sql = sqlite::open(path).unwrap();
                    sql.execute("PRAGMA foreign_keys=ON; BEGIN; INSERT INTO child VALUES(42)")
                        .unwrap();
                    let _permit = token.begin_commit()?;
                    step.enter(&token);
                    assert!(sql.execute("COMMIT").is_err());
                    sql.execute("ROLLBACK").unwrap();
                    Err::<(), _>(ErrorCode::InvalidParams)
                })
                .await?;
            unreachable!()
        })
    });
    client.request("job").await;
    bounded(step.entered.notified()).await;
    client.cancel("job", true).await;
    client.health().await;
    step.gate.release();
    assert_eq!(client.next().await["error"]["code"], "invalid_params");
    assert!(step.exited.load(Ordering::SeqCst));
    let sql = sqlite::open(path).unwrap();
    let mut query = sql.prepare("SELECT COUNT(*) FROM child").unwrap();
    assert_eq!(query.next().unwrap(), sqlite::State::Row);
    assert_eq!(query.read::<i64, _>(0).unwrap(), 0);
    client.finish().await.unwrap();
}

#[tokio::test]
async fn mutation_error_allows_local_repair_but_never_hides_original_error() {
    let repaired = Arc::new(AtomicBool::new(false));
    let completed = repaired.clone();
    let mut client = Client::new(move |_, context| {
        let completed = completed.clone();
        Box::pin(async move {
            let group = context.operation_group()?;
            assert_eq!(
                group
                    .run_mutation("fails", |_| async {
                        Err::<(), _>(ErrorCode::InvalidParams)
                    })
                    .await,
                Err(ErrorCode::InvalidParams)
            );
            group
                .run_compensation("local-repair", |token| async move {
                    let _permit = token.begin_commit()?;
                    Ok(())
                })
                .await?;
            completed.store(true, Ordering::SeqCst);
            Err(ErrorCode::Cancelled)
        })
    });
    client.request("job").await;
    assert_eq!(client.next().await["error"]["code"], "invalid_params");
    assert!(repaired.load(Ordering::SeqCst));
    client.finish().await.unwrap();
}

#[tokio::test]
async fn false_cancellation_unsealed_success_and_recursive_work_are_rejected() {
    for mode in ["cancel", "success", "recursive", "incomplete-reset"] {
        let checked = Arc::new(AtomicBool::new(false));
        let passed = checked.clone();
        let mut client = Client::new(move |_, context| {
            let passed = passed.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                if mode == "recursive" {
                    let nested = group.clone();
                    group
                        .run_preparation("outer", move |_| async move {
                            assert_eq!(
                                nested.run_preparation("nested", |_| async { Ok(()) }).await,
                                Err::<(), _>(ErrorCode::Internal)
                            );
                            Ok(())
                        })
                        .await?;
                }
                if mode == "incomplete-reset" {
                    let _plan = group.begin_irreversible_plan("intent".into())?;
                }
                passed.store(true, Ordering::SeqCst);
                if mode == "cancel" {
                    Err(ErrorCode::Cancelled)
                } else {
                    Ok(json!({}))
                }
            })
        });
        client.request("job").await;
        assert_eq!(client.next().await["error"]["code"], "internal");
        assert!(checked.load(Ordering::SeqCst));
        client.finish().await.unwrap();
    }
}

#[tokio::test]
async fn wire_ack_precedes_terminal_and_no_queued_progress_follows_accepted_ack() {
    let ready = Arc::new(Notify::new());
    let signal = ready.clone();
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let mut client = Client::new(move |_, context| {
        let signal = signal.clone();
        let gate = gate.clone();
        Box::pin(async move {
            signal.notify_one();
            gate.acquire().await.unwrap().forget();
            for _ in 0..QUEUE_CAPACITY * 3 {
                context.emit("queued", json!({})).await?;
            }
            Err(ErrorCode::Cancelled)
        })
    });
    client.request("job").await;
    bounded(ready.notified()).await;
    client.output.pause.store(true, Ordering::SeqCst);
    release.add_permits(1);
    bounded(client.output.observed.notified()).await;
    client.send(json!({"v":1,"type":"cancel","id":"job"})).await;
    client.output.pause.store(false, Ordering::SeqCst);
    client.output.waker.wake();
    let mut sequence = 0;
    loop {
        let frame = client.next().await;
        if frame["type"] == "cancel_ack" {
            assert_eq!(frame["accepted"], true);
            break;
        }
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["seq"], sequence);
        sequence += 1;
    }
    let frame = client.next().await;
    assert_eq!(frame["type"], "cancelled");
    assert_eq!(frame["seq"], sequence);
    client.finish().await.unwrap();
}

#[tokio::test]
async fn irreversible_native_plan_drains_on_eof_and_broken_output_without_accepting_stop() {
    for mode in ["connected", "eof", "output"] {
        let (_root, path) = database();
        let db = path.clone();
        let marker = Arc::new(Step::default());
        let native = marker.clone();
        let mut client = Client::new(move |_, context| {
            let db = db.clone();
            let marker = native.clone();
            Box::pin(async move {
                let group = context.operation_group()?;
                // Fake trusted intent: only runtime ordering/joins are tested.
                let plan = group.begin_irreversible_plan("fixture-intent".into())?;
                for (index, step) in RESET_STEPS.into_iter().enumerate() {
                    let gate = if index == 0 {
                        marker.clone()
                    } else {
                        let step = Arc::new(Step::default());
                        step.gate.release();
                        step
                    };
                    plan.run_step(
                        step,
                        sql_mutation(db.clone(), gate.clone(), true, index as i64 + 1),
                    )
                    .await?;
                    assert!(gate.exited.load(Ordering::SeqCst));
                    assert!(!group.preparation_stop().cancel());
                }
                let _permit = plan.seal_completion()?;
                Ok(json!({"steps":6}))
            })
        });
        client.request("job").await;
        bounded(marker.entered.notified()).await;
        client.cancel("job", false).await;
        client.health().await;
        if mode == "eof" {
            client.writer.shutdown().await.unwrap();
        }
        if mode == "output" {
            client.output.fail.store(true, Ordering::SeqCst);
            client
                .send(json!({"v":1,"type":"request","id":"probe","command":"health","params":{}}))
                .await;
            bounded(client.output.observed.notified()).await;
        }
        assert!(!client.task.is_finished());
        marker.gate.release();
        if mode == "output" {
            assert_eq!(
                bounded(client.task).await.unwrap().unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
        } else {
            assert_eq!(client.next().await["data"]["steps"], 6);
            client.finish().await.unwrap();
        }
        assert!(marker.exited.load(Ordering::SeqCst));
        assert_eq!(rows(&path), vec![1, 2, 3, 4, 5, 6]);
    }
}
