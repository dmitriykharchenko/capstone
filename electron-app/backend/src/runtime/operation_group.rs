//! Request-owned cooperative work, not a storage transaction/recovery system.
//!
//! Trusted handlers must await every run_* result, join any workers they create
//! inside that closure, and return Cancelled ONLY after durable reconciliation is
//! resolved. Compensation closures must be bounded local journal-derived repairs,
//! never provider retries, PDF/history writes or arbitrary cancellation bypasses.
//! A runtime cannot inspect those domain invariants. No syscall preemption, replay,
//! or graceful-shutdown guarantee on forced serve/runtime/process destruction.

use super::{CANCELLABLE, CANCELLED, COMMITTING, Cancellation, CommitPermit};
use crate::{protocol::ErrorCode, registry::BackendResult};
use std::{
    cell::Cell,
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, Ordering},
    },
    thread::JoinHandle,
};
use tokio::sync::oneshot;

/// A hard per-request preparation bound, independent of a handler's lower limit.
const MAX_PREPARATIONS: usize = 32;
thread_local! {
    static WORKER_GROUP: Cell<*const Shared> = const { Cell::new(std::ptr::null()) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    StopRequested,
    Finalizing,
    FinalSealed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Preparation,
    Mutation,
    Compensation,
    Reset(ResetStep),
}

struct Worker {
    kind: Kind,
    token: Arc<AtomicU8>,
    thread: Option<JoinHandle<BackendResult<()>>>,
}

struct State {
    phase: Phase,
    closed: bool,
    next_id: u64,
    workers: BTreeMap<u64, Worker>,
    failure: Option<ErrorCode>,
    abandoned: bool,
    reset_next: usize,
}

struct Shared {
    parent: Arc<AtomicU8>,
    state: Mutex<State>,
    // Join ownership is serialized separately: never hold state while joining a
    // thread that may need it to check/seal/cancel its linked token.
    drain: Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub(super) struct CancellationLink {
    shared: Arc<Shared>,
    kind: Kind,
}

impl CancellationLink {
    pub(super) fn cancel(&self, token: &Arc<AtomicU8>) -> bool {
        let mut state = lock(&self.shared.state);
        if self.kind == Kind::Preparation {
            return self.shared.stop(&mut state);
        }
        // Cleanup/finalization cannot be cancelled a second time.
        if matches!(self.kind, Kind::Compensation | Kind::Reset(_)) {
            return false;
        }
        matches!(
            token.compare_exchange(CANCELLABLE, CANCELLED, Ordering::AcqRel, Ordering::Acquire),
            Ok(_) | Err(CANCELLED)
        )
    }

    pub(super) fn begin_commit(&self, token: &Arc<AtomicU8>) -> BackendResult<CommitPermit> {
        let state = lock(&self.shared.state);
        if self.kind == Kind::Preparation {
            return Err(if state.phase == Phase::StopRequested {
                ErrorCode::Cancelled
            } else {
                ErrorCode::Internal
            });
        }
        if !state
            .workers
            .values()
            .any(|worker| Arc::ptr_eq(&worker.token, token))
        {
            return Err(if token.load(Ordering::Acquire) == CANCELLED {
                ErrorCode::Cancelled
            } else {
                ErrorCode::Internal
            });
        }
        // The child CAS and parent stop/admission share this lock. Stop cannot
        // win in the gap between reading the parent and sealing the child.
        token
            .compare_exchange(CANCELLABLE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| CommitPermit { _private: () })
            .map_err(|s| {
                if s == CANCELLED {
                    ErrorCode::Cancelled
                } else {
                    ErrorCode::Internal
                }
            })
    }
}

impl Shared {
    fn stop(&self, state: &mut State) -> bool {
        if matches!(state.phase, Phase::Finalizing | Phase::FinalSealed) {
            return false;
        }
        state.phase = Phase::StopRequested;
        self.parent.store(CANCELLED, Ordering::Release);
        for worker in state.workers.values() {
            if matches!(worker.kind, Kind::Mutation | Kind::Preparation) {
                let _ = worker.token.compare_exchange(
                    CANCELLABLE,
                    CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        }
        true
    }

    // Must hold drain. The slot stays registered until actual thread exit, not
    // merely oneshot send, so neither sealing nor another mutation can race join.
    fn join(&self, id: u64) -> BackendResult<()> {
        let thread = lock(&self.state)
            .workers
            .get_mut(&id)
            .and_then(|worker| worker.thread.take())
            .ok_or(ErrorCode::Internal)?;
        let mut result = thread.join().unwrap_or(Err(ErrorCode::Internal));
        let mut state = lock(&self.state);
        let worker = state.workers.remove(&id).ok_or(ErrorCode::Internal)?;
        // Like a final request seal, a child's seal requires its actual outcome.
        // A stopped parent does not license an invented rollback for that child.
        if result == Err(ErrorCode::Cancelled)
            && worker.kind != Kind::Preparation
            && worker.token.load(Ordering::Acquire) == COMMITTING
        {
            result = Err(ErrorCode::Internal);
        }
        if worker.kind != Kind::Preparation {
            let _ = worker.token.compare_exchange(
                CANCELLABLE,
                CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        if let Err(code) = result {
            if code != ErrorCode::Cancelled
                || matches!(worker.kind, Kind::Compensation | Kind::Reset(_))
            {
                state.failure = Some(if code == ErrorCode::Cancelled {
                    ErrorCode::Internal
                } else {
                    code
                });
            }
            self.stop(&mut state);
        } else if matches!(worker.kind, Kind::Reset(_)) {
            state.reset_next += 1;
        }
        result
    }
}

/// Clones refer to ONE runtime-owned group; dropping a clone never detaches work.
#[derive(Clone)]
pub struct OperationGroup(Arc<Shared>);

#[derive(Debug)]
#[must_use = "the completion seal is sticky; report the actual final outcome"]
pub struct CompletionPermit {
    _private: (),
}

/// Fixed, ordered reset capabilities. Successful completion of PersistFinalizing
/// MUST mean the durable marker has been verified, not merely attempted. The
/// runtime checks sequence/ownership, not marker truth or database contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetStep {
    PersistFinalizing,
    ResetRetrieval,
    RecordRetrieval,
    ResetGraph,
    RecordGraph,
    CompleteJournal,
}
const RESET_STEPS: [ResetStep; 6] = [
    ResetStep::PersistFinalizing,
    ResetStep::ResetRetrieval,
    ResetStep::RecordRetrieval,
    ResetStep::ResetGraph,
    ResetStep::RecordGraph,
    ResetStep::CompleteJournal,
];

pub struct Finalization {
    group: OperationGroup,
    reset_op_id: String,
}

impl OperationGroup {
    pub(super) fn new() -> Self {
        Self(Arc::new(Shared {
            parent: Arc::new(AtomicU8::new(CANCELLABLE)),
            state: Mutex::new(State {
                phase: Phase::Open,
                closed: false,
                next_id: 0,
                workers: BTreeMap::new(),
                failure: None,
                abandoned: false,
                reset_next: 0,
            }),
            drain: Mutex::new(()),
        }))
    }

    /// Read/provider preparation signal. It cannot grant a mutation commit seal.
    pub fn preparation_stop(&self) -> Cancellation {
        Cancellation {
            state: self.0.parent.clone(),
            link: Some(CancellationLink {
                shared: self.0.clone(),
                kind: Kind::Preparation,
            }),
        }
    }

    pub fn check(&self) -> BackendResult<()> {
        let state = lock(&self.0.state);
        if state.abandoned {
            return Err(ErrorCode::Internal);
        }
        if let Some(code) = state.failure {
            return Err(code);
        }
        if state.phase == Phase::StopRequested {
            return Err(ErrorCode::Cancelled);
        }
        if state.closed || state.phase != Phase::Open {
            return Err(ErrorCode::Internal);
        }
        Ok(())
    }

    /// Supervise read/provider work. All nested native workers must be joined by
    /// the supplied future. Up to 32 preparations and one mutation are retained.
    pub async fn run_preparation<T, F, Fut>(&self, label: &'static str, work: F) -> BackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = BackendResult<T>> + Send + 'static,
    {
        self.run(label, Kind::Preparation, work).await
    }

    /// Each mutation gets a distinct linked single-commit token. A sealed child
    /// finishes with its actual outcome; it never seals this whole request.
    pub async fn run_mutation<T, F, Fut>(&self, label: &'static str, work: F) -> BackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = BackendResult<T>> + Send + 'static,
    {
        self.run(label, Kind::Mutation, work).await
    }

    /// Only after stop/error; trusted local durable-artifact repair, not a general
    /// uncancellable execution API. Errors remain sticky even if the caller ignores them.
    pub async fn run_compensation<T, F, Fut>(
        &self,
        label: &'static str,
        work: F,
    ) -> BackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = BackendResult<T>> + Send + 'static,
    {
        self.run(label, Kind::Compensation, work).await
    }

    async fn run<T, F, Fut>(&self, label: &'static str, kind: Kind, work: F) -> BackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = BackendResult<T>> + Send + 'static,
    {
        // A worker must not recursively wait for this group's join lane. Such a
        // dependency could self-deadlock when the supervisor drains its parent.
        if WORKER_GROUP.with(|current| current.get() == Arc::as_ptr(&self.0)) {
            return Err(ErrorCode::Internal);
        }
        let (tx, rx) = oneshot::channel();
        let runtime = tokio::runtime::Handle::current();
        let id = {
            let mut state = lock(&self.0.state);
            if state.closed
                || (state.abandoned && kind != Kind::Compensation)
                || label.is_empty()
                || label.len() > 128
            {
                return Err(ErrorCode::Internal);
            }
            match kind {
                Kind::Preparation | Kind::Mutation => {
                    if let Some(code) = state.failure {
                        return Err(code);
                    }
                    if state.phase == Phase::StopRequested {
                        return Err(ErrorCode::Cancelled);
                    }
                    if state.phase != Phase::Open {
                        return Err(ErrorCode::Internal);
                    }
                }
                Kind::Compensation if state.phase != Phase::StopRequested => {
                    return Err(ErrorCode::Internal);
                }
                Kind::Reset(step)
                    if state.phase != Phase::Finalizing
                        || state.failure.is_some()
                        || RESET_STEPS.get(state.reset_next) != Some(&step) =>
                {
                    return Err(ErrorCode::Internal);
                }
                _ => {}
            }
            if kind == Kind::Preparation {
                if state
                    .workers
                    .values()
                    .filter(|w| w.kind == Kind::Preparation)
                    .count()
                    >= MAX_PREPARATIONS
                {
                    return Err(ErrorCode::Busy);
                }
            } else if state.workers.values().any(|w| w.kind != Kind::Preparation) {
                return Err(ErrorCode::Internal);
            }
            let token = if kind == Kind::Preparation {
                self.0.parent.clone()
            } else {
                Arc::new(AtomicU8::new(CANCELLABLE))
            };
            let cancellation = Cancellation {
                state: token.clone(),
                link: Some(CancellationLink {
                    shared: self.0.clone(),
                    kind,
                }),
            };
            let id = state.next_id;
            state.next_id = id.checked_add(1).ok_or(ErrorCode::Internal)?;
            let shared = self.0.clone();
            let thread = match std::thread::Builder::new()
                .name("operation-group".into())
                .spawn(move || {
                    WORKER_GROUP.with(|current| current.set(Arc::as_ptr(&shared)));
                    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        runtime.block_on(async { work(cancellation).await })
                    }))
                    .unwrap_or(Err(ErrorCode::Internal));
                    let status = result.as_ref().map(|_| ()).map_err(|code| *code);
                    let _ = tx.send(result);
                    status
                }) {
                Ok(thread) => thread,
                Err(_) => {
                    state.failure = Some(ErrorCode::Internal);
                    self.0.stop(&mut state);
                    return Err(ErrorCode::Internal);
                }
            };
            state.workers.insert(
                id,
                Worker {
                    kind,
                    token,
                    thread: Some(thread),
                },
            );
            id
        };
        let mut waiter = Waiter {
            group: self.clone(),
            id,
            resolved: false,
        };
        let result = rx.await.unwrap_or(Err(ErrorCode::Internal));
        let joined = {
            let _drain = lock(&self.0.drain);
            self.0.join(id)
        };
        waiter.resolved = true;
        joined?;
        result
    }

    pub fn seal_completion(&self) -> BackendResult<CompletionPermit> {
        self.seal(false)
    }

    fn seal(&self, reset: bool) -> BackendResult<CompletionPermit> {
        let mut state = lock(&self.0.state);
        if let Some(code) = state.failure {
            return Err(code);
        }
        if state.phase == Phase::StopRequested {
            return Err(ErrorCode::Cancelled);
        }
        let expected = if reset {
            Phase::Finalizing
        } else {
            Phase::Open
        };
        if state.closed
            || state.abandoned
            || state.phase != expected
            || !state.workers.is_empty()
            || (reset && state.reset_next != RESET_STEPS.len())
        {
            return Err(ErrorCode::Internal);
        }
        state.phase = Phase::FinalSealed;
        self.0.parent.store(COMMITTING, Ordering::Release);
        Ok(CompletionPermit { _private: () })
    }

    /// Caller must already own a verified durable reset_prepared intent and the
    /// exclusive derived-store gate. This transition refuses subsequent stop;
    /// it does NOT assert the later reset_finalizing marker exists on disk.
    pub fn begin_irreversible_plan(&self, reset_op_id: String) -> BackendResult<Finalization> {
        let mut state = lock(&self.0.state);
        if state.phase == Phase::StopRequested {
            return Err(ErrorCode::Cancelled);
        }
        if state.closed
            || state.abandoned
            || state.failure.is_some()
            || state.phase != Phase::Open
            || !state.workers.is_empty()
            || reset_op_id.is_empty()
            || reset_op_id.len() > 256
        {
            return Err(ErrorCode::Internal);
        }
        state.phase = Phase::Finalizing;
        self.0.parent.store(COMMITTING, Ordering::Release);
        Ok(Finalization {
            group: self.clone(),
            reset_op_id,
        })
    }

    // Called on the retained blocking request runner, even after handler panic.
    // Close admission before draining: leaked clones cannot create later work.
    pub(super) fn finish<T>(&self, result: BackendResult<T>) -> BackendResult<T> {
        let _drain = lock(&self.0.drain);
        let ids = {
            let mut state = lock(&self.0.state);
            state.closed = true;
            if !state.workers.is_empty() {
                state.abandoned = true;
                self.0.stop(&mut state);
            }
            state.workers.keys().copied().collect::<Vec<_>>()
        };
        for id in ids {
            let _ = self.0.join(id);
        }
        let state = lock(&self.0.state);
        if state.abandoned {
            return Err(ErrorCode::Internal);
        }
        if let Some(code) = state.failure {
            return Err(code);
        }
        match result {
            Ok(_) if state.phase != Phase::FinalSealed => Err(ErrorCode::Internal),
            Err(ErrorCode::Cancelled) if state.phase != Phase::StopRequested => {
                Err(ErrorCode::Internal)
            }
            other => other,
        }
    }
}

// Dropping/aborting a run_* future stops admission, records unresolved work as
// Internal, and joins its real worker. This can block: like existing native Drop
// guards it must be polled/dropped on a blocking-capable runner, not a UI executor.
struct Waiter {
    group: OperationGroup,
    id: u64,
    resolved: bool,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        {
            let mut state = lock(&self.group.0.state);
            state.abandoned = true;
            self.group.0.stop(&mut state);
        }
        let _drain = lock(&self.group.0.drain);
        let _ = self.group.0.join(self.id);
    }
}

impl Finalization {
    pub fn reset_op_id(&self) -> &str {
        &self.reset_op_id
    }

    /// Only the next declared step can start. A failure forbids further reset
    /// work; durable inspection/poison and later recovery belong to the handler.
    pub async fn run_step<T, F, Fut>(&self, step: ResetStep, work: F) -> BackendResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = BackendResult<T>> + Send + 'static,
    {
        self.group.run("reset", Kind::Reset(step), work).await
    }

    pub fn seal_completion(&self) -> BackendResult<CompletionPermit> {
        self.group.seal(true)
    }
}
