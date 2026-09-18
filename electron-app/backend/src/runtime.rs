use std::{
    collections::{BTreeMap, HashSet},
    io,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
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

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn check(&self) -> BackendResult<()> {
        if self.0.load(Ordering::Acquire) {
            Err(ErrorCode::Cancelled)
        } else {
            Ok(())
        }
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

// Modules emit public progress only. The dispatcher alone assigns sequence numbers
// and terminal outcomes. Blocking work must be offloaded and check cancellation
// before each mutation; dropping a future cannot stop an already-running syscall.
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
            .map_err(|_| ErrorCode::Cancelled)
    }
}

enum WorkerMessage {
    Event(String, OutputBody),
    Finished(String, BackendResult<Value>),
}

struct Active {
    seq: u64,
    cancellation: Cancellation,
    task: JoinHandle<()>,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
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
    let mut seen = HashSet::new();
    loop {
        tokio::select! {
            incoming = input_rx.recv() => {
                let Some(incoming) = incoming else { break; };
                let input = match incoming? {
                    Ok(input) => input,
                    Err(code) => { write_output(&mut writer, Output::protocol_error(code)).await?; continue; }
                };
                match input {
                    Input::Cancel { id, .. } => {
                        let cancelled = active.remove(&id);
                        let accepted = cancelled.is_some();
                        if let Some(job) = cancelled {
                            let seq = job.seq;
                            drop(job);
                            write_output(&mut writer, Output::request(&id, seq, OutputBody::Cancelled)).await?;
                        }
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
                        let task = tokio::spawn(async move {
                            let outcome = AssertUnwindSafe(async { handler.call(params, context).await }).catch_unwind().await.unwrap_or(Err(ErrorCode::Internal));
                            // Bound retained worker results before queueing them, not
                            // merely when stdout eventually becomes writable.
                            let outcome = outcome.and_then(|data| {
                                if serde_json::to_vec(&data).map_err(|_| ErrorCode::Internal)?.len() > MAX_FRAME_BYTES - 256 {
                                    Err(ErrorCode::ResponseTooLarge)
                                } else { Ok(data) }
                            });
                            let _ = sender.send(WorkerMessage::Finished(request_id, outcome)).await;
                        });
                        active.insert(id, Active { seq: 0, cancellation, task });
                    }
                }
            }
            Some(event) = events.recv() => {
                match event {
                    WorkerMessage::Event(id, body) => {
                        if let Some(job) = active.get_mut(&id) {
                            write_output(&mut writer, Output::request(&id, job.seq, body)).await?;
                            job.seq += 1;
                        }
                    }
                    WorkerMessage::Finished(id, result) => {
                        if let Some(job) = active.remove(&id) {
                            let body = match result {
                                Ok(data) => OutputBody::Result { data },
                                Err(ErrorCode::Cancelled) => OutputBody::Cancelled,
                                Err(code) => error_body(code),
                            };
                            write_output(&mut writer, Output::request(&id, job.seq, body)).await?;
                        }
                    }
                }
            }
        }
    }
    // EOF is owner shutdown, not permission to keep mutating after Electron exits.
    let cancelled: Vec<_> = active
        .iter()
        .map(|(id, job)| (id.clone(), job.seq))
        .collect();
    drop(active);
    for (id, seq) in cancelled {
        write_output(
            &mut writer,
            Output::request(&id, seq, OutputBody::Cancelled),
        )
        .await?;
    }
    Ok(())
}
