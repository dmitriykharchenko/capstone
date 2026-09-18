use std::{
    collections::{BTreeMap, HashSet},
    io,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use futures_util::{
    StreamExt,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{
    protocol::*,
    registry::{BackendResult, Registry},
};

const CANCELLABLE: u8 = 0;
const CANCELLED: u8 = 1;
const COMMITTING: u8 = 2;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicU8>);

/// Proof that this request won its one-way final-commit boundary.
///
/// Dropping this permit does NOT unseal the request, even on failure or panic.
/// The shared cancellation state protects the handler and its queued result all
/// the way through terminal publication. A permit is not proof of durable success.
#[derive(Debug)]
#[must_use = "acquire immediately before the final commit, then report its actual outcome"]
pub struct CommitPermit {
    _private: (),
}

impl Cancellation {
    pub fn check(&self) -> BackendResult<()> {
        if self.0.load(Ordering::Acquire) == CANCELLED {
            Err(ErrorCode::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Signal cancellation, returning whether cancellation has won. Idempotent;
    /// returns false once final commit is sealed. Also usable by native tests.
    pub fn cancel(&self) -> bool {
        matches!(
            self.0
                .compare_exchange(CANCELLABLE, CANCELLED, Ordering::AcqRel, Ordering::Acquire),
            Ok(_) | Err(CANCELLED)
        )
    }

    /// Atomically seal the request immediately before its irreversible final
    /// COMMIT/atomic publication. Cancellation wins => no commit may start.
    /// Commit wins => check() stays Ok and cancellation is rejected permanently.
    /// A second begin_commit is a programming error, not another permit.
    ///
    /// Join offloaded native work and return the real result/error. Do not detach
    /// it, retry a mutation on transport loss, or use this seal for intermediate
    /// indexing transactions: partial-progress cancellation is a domain contract.
    pub fn begin_commit(&self) -> BackendResult<CommitPermit> {
        self.0
            .compare_exchange(CANCELLABLE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| CommitPermit { _private: () })
            .map_err(|state| {
                if state == CANCELLED {
                    ErrorCode::Cancelled
                } else {
                    ErrorCode::Internal
                }
            })
    }

    fn is_committing(&self) -> bool {
        self.0.load(Ordering::Acquire) == COMMITTING
    }
}

// Modules emit public progress only. The dispatcher alone assigns sequence numbers
// and terminal outcomes. Blocking work must be offloaded and check cancellation
// before each mutation, then seal before final commit; a check alone races cancel.
#[derive(Clone)]
pub struct RequestContext {
    id: String,
    streaming: bool,
    cancellation: Cancellation,
    messages: mpsc::Sender<WorkerMessage>,
}

impl RequestContext {
    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub async fn emit(&self, event: &'static str, data: Value) -> BackendResult<()> {
        self.cancellation.check()?;
        if !self.streaming {
            return Ok(());
        }
        let body = OutputBody::Event {
            event: event.to_owned(),
            data,
        };
        let probe = Output::request(&self.id, u64::MAX, body);
        let encoded = serde_json::to_vec(&probe).map_err(|_| ErrorCode::Internal)?;
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(ErrorCode::ResponseTooLarge);
        }
        self.messages
            .send(WorkerMessage::Event(self.id.clone(), probe.body))
            .await
            .map_err(|_| {
                if self.cancellation.is_committing() {
                    ErrorCode::Internal
                } else {
                    ErrorCode::Cancelled
                }
            })
    }
}

enum WorkerMessage {
    Event(String, OutputBody),
    Finished(String, BackendResult<Value>),
}

struct Active {
    seq: u64,
    cancellation: Cancellation,
    abort: AbortHandle,
}

impl Active {
    fn cancel(&self) -> bool {
        if self.cancellation.cancel() {
            self.abort.abort();
            true
        } else {
            false
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ReaderGuard(JoinHandle<()>);
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn write_output<W: AsyncWrite + Unpin>(writer: &mut W, mut output: Output) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&output)?;
    if bytes.len() > MAX_FRAME_BYTES {
        output.body = OutputBody::Error {
            error: ErrorCode::ResponseTooLarge.into(),
        };
        bytes = serde_json::to_vec(&output)?;
    }
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), async {
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "protocol output stalled"))?
}

fn error_body(code: ErrorCode) -> OutputBody {
    OutputBody::Error { error: code.into() }
}

fn terminal_body(result: BackendResult<Value>, cancellation: &Cancellation) -> OutputBody {
    if cancellation.check().is_err() {
        return OutputBody::Cancelled;
    }
    match result {
        Ok(data) => OutputBody::Result { data },
        // A sealed handler cannot claim cancellation/rollback after committing.
        Err(ErrorCode::Cancelled) if cancellation.is_committing() => {
            error_body(ErrorCode::Internal)
        }
        Err(ErrorCode::Cancelled) => OutputBody::Cancelled,
        Err(code) => error_body(code),
    }
}

async fn publish_if_connected<W: AsyncWrite + Unpin>(
    writer: &mut W,
    output: Output,
    transport: &mut io::Result<()>,
) {
    if transport.is_ok() {
        *transport = write_output(writer, output).await;
    }
}

/// Serve until input closes or transport fails. Shutdown cancels ordinary jobs
/// and joins ALL admitted jobs without a native-work timeout: stopping an in-flight
/// native call cannot prove teardown/rollback. Callers must await this future for graceful
/// shutdown; forced process termination or lost output leaves the outcome unknown.
pub async fn serve<R, W>(reader: R, mut writer: W, registry: Registry) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    // A dedicated bounded reader preserves partial frames across dispatcher selects.
    let (input_tx, mut input_rx) = mpsc::channel(1);
    let _reader = ReaderGuard(tokio::spawn(async move {
        let mut reader = BufReader::with_capacity(8192, reader);
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(frame)) => {
                    if input_tx
                        .send(Ok(frame.and_then(|bytes| Input::parse(&bytes))))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = input_tx.send(Err(error)).await;
                    break;
                }
            }
        }
    }));
    let (messages, mut events) = mpsc::channel(QUEUE_CAPACITY);
    let mut active: BTreeMap<String, Active> = BTreeMap::new();
    let mut joins = FuturesUnordered::new();
    let mut seen = HashSet::new();
    let mut input_error = None;
    let mut transport = async {
    loop {
        tokio::select! {
            incoming = input_rx.recv() => {
                let Some(incoming) = incoming else { break; };
                let incoming = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => { input_error = Some(error); break; }
                };
                let input = match incoming {
                    Ok(input) => input,
                    Err(code) => { write_output(&mut writer, Output::protocol_error(code)).await?; continue; }
                };
                match input {
                    Input::Cancel { id, .. } => {
                        // Retain capacity and ownership until the request runner has
                        // dropped its future AND joined any owned native work.
                        let accepted = active.get(&id).is_some_and(Active::cancel);
                        write_output(&mut writer, Output {v: VERSION, id: Some(id), seq: None, body: OutputBody::CancelAck { accepted }}).await?;
                    }
                    Input::Request { id, command, params, .. } => {
                        if seen.contains(&id) {
                            write_output(&mut writer, Output::protocol_error(ErrorCode::DuplicateId)).await?;
                            continue;
                        }
                        if seen.len() == MAX_SESSION_REQUESTS {
                            write_output(&mut writer, Output::protocol_error(ErrorCode::SessionLimit)).await?;
                            break;
                        }
                        seen.insert(id.clone());
                        // Health/control traffic remains responsive even at the work limit.
                        if command == "health" {
                            let body = if params.as_object().is_some_and(|p| p.is_empty()) {
                                OutputBody::Result { data: registry.health() }
                            } else { error_body(ErrorCode::InvalidParams) };
                            write_output(&mut writer, Output::request(&id, 0, body)).await?;
                            continue;
                        }
                        if active.len() >= MAX_ACTIVE_REQUESTS {
                            write_output(&mut writer, Output::request(&id, 0, error_body(ErrorCode::Busy))).await?;
                            continue;
                        }
                        let handler = match registry.handler(&command) {
                            Ok(handler) => handler,
                            Err(code) => {
                                write_output(&mut writer, Output::request(&id, 0, error_body(code))).await?;
                                continue;
                            }
                        };
                        let cancellation = Cancellation::default();
                        let context = RequestContext { id: id.clone(), streaming: registry.is_streaming(&command), cancellation: cancellation.clone(), messages: messages.clone() };
                        let sender = messages.clone();
                        let request_id = id.clone();
                        let (abort, registration) = AbortHandle::new_pair();
                        let runtime = tokio::runtime::Handle::current();
                        // Bounded by MAX_ACTIVE_REQUESTS, including cancellation
                        // drains. Native join-on-drop can block; never poll/drop a
                        // handler on the dispatcher executor (even current-thread).
                        // Abortable drops immediately without weakening arbitrary
                        // injected handlers' cancellation. The blocking JoinHandle
                        // itself is NEVER aborted or detached on graceful shutdown.
                        let task = tokio::task::spawn_blocking(move || {
                            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                runtime.block_on(Abortable::new(async {
                                    handler.call(params, context).await
                                }, registration))
                            })).unwrap_or(Ok(Err(ErrorCode::Internal)))
                                .unwrap_or(Err(ErrorCode::Cancelled));
                            // Bound retained worker results before queueing them, not
                            // merely when stdout eventually becomes writable.
                            outcome.and_then(|data| {
                                if serde_json::to_vec(&data).map_err(|_| ErrorCode::Internal)?.len() > MAX_FRAME_BYTES - 256 {
                                    Err(ErrorCode::ResponseTooLarge)
                                } else { Ok(data) }
                            })
                        });
                        joins.push(async move {
                            let outcome = task.await.unwrap_or(Err(ErrorCode::Internal));
                            // Same bounded FIFO as events: every preceding event
                            // is consumed before this sole, post-teardown terminal.
                            let _ = sender.send(WorkerMessage::Finished(request_id, outcome)).await;
                        });
                        active.insert(id, Active { seq: 0, cancellation, abort });
                    }
                }
            }
            Some(event) = events.recv() => {
                match event {
                    WorkerMessage::Event(id, body) => {
                        if let Some(job) = active.get_mut(&id).filter(|job| job.cancellation.check().is_ok()) {
                            write_output(&mut writer, Output::request(&id, job.seq, body)).await?;
                            job.seq += 1;
                        }
                    }
                    WorkerMessage::Finished(id, result) => {
                        if let Some(job) = active.remove(&id) {
                            let body = terminal_body(result, &job.cancellation);
                            write_output(&mut writer, Output::request(&id, job.seq, body)).await?;
                        }
                    }
                }
            }
            Some(()) = joins.next(), if !joins.is_empty() => {}
        }
    }
    Ok(())
    }.await;
    // Stop admitting input, then decide ALL pre-commit cancellations before any
    // potentially stalled output. A concurrent begin_commit has the same CAS winner.
    drop(_reader);
    for job in active.values() {
        job.cancel();
    }
    // Keep consuming bounded worker queues even with broken stdout, so a sealed
    // handler can finish. Never abort it or manufacture cancellation/success.
    while !active.is_empty() || !joins.is_empty() {
        tokio::select! {
        Some(()) = joins.next(), if !joins.is_empty() => {}
        Some(event) = events.recv() => match event {
            WorkerMessage::Event(id, body) => {
                if let Some(job) = active.get_mut(&id).filter(|job| job.cancellation.check().is_ok()) {
                    publish_if_connected(
                        &mut writer,
                        Output::request(&id, job.seq, body),
                        &mut transport,
                    )
                    .await;
                    job.seq += 1;
                }
            }
            WorkerMessage::Finished(id, result) => {
                if let Some(job) = active.remove(&id) {
                    let body = terminal_body(result, &job.cancellation);
                    publish_if_connected(
                        &mut writer,
                        Output::request(&id, job.seq, body),
                        &mut transport,
                    )
                    .await;
                }
            }
        }
        }
    }
    transport.and(input_error.map_or(Ok(()), Err))
}
