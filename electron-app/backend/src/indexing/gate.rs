use super::{IndexError, IndexResult};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Readiness {
    Starting,
    Recovering,
    Ready,
    Poisoned,
}

/// One instance per assembled derived store. Lock order is job -> gate -> one
/// store transaction. Never acquire the job lock from inside a read guard.
#[derive(Clone)]
pub struct DerivedStoreGate {
    job: Arc<Mutex<()>>,
    lock: Arc<RwLock<()>>,
    state: Arc<AtomicU8>,
}
impl Default for DerivedStoreGate {
    fn default() -> Self {
        Self::new()
    }
}
impl DerivedStoreGate {
    pub fn new() -> Self {
        Self {
            job: Arc::new(Mutex::new(())),
            lock: Arc::new(RwLock::new(())),
            state: Arc::new(AtomicU8::new(Readiness::Starting as u8)),
        }
    }
    /// Diagnostic only; admission always checks again under the lock.
    pub fn readiness(&self) -> Readiness {
        match self.state.load(Ordering::Acquire) {
            0 => Readiness::Starting,
            1 => Readiness::Recovering,
            2 => Readiness::Ready,
            _ => Readiness::Poisoned,
        }
    }
    pub async fn read(&self) -> IndexResult<RwLockReadGuard<'_, ()>> {
        let guard = self.lock.read().await;
        self.require_ready()?;
        Ok(guard)
    }
    fn require_ready(&self) -> IndexResult<()> {
        match self.readiness() {
            Readiness::Ready => Ok(()),
            Readiness::Poisoned => Err(IndexError::Poisoned),
            _ => Err(IndexError::NotReady),
        }
    }
    pub(super) fn poison(&self) {
        self.state
            .store(Readiness::Poisoned as u8, Ordering::Release);
    }
    pub(super) async fn write(&self, recovery: bool) -> IndexResult<WriteGuard> {
        let job = self.job.clone().lock_owned().await;
        let lock = self.lock.clone().write_owned().await;
        if !recovery {
            self.require_ready()?;
        }
        if recovery {
            self.state
                .store(Readiness::Recovering as u8, Ordering::Release);
        }
        Ok(WriteGuard {
            gate: self.clone(),
            _lock: lock,
            _job: job,
            unresolved: recovery,
        })
    }
}
pub(super) struct WriteGuard {
    gate: DerivedStoreGate,
    _lock: OwnedRwLockWriteGuard<()>,
    _job: OwnedMutexGuard<()>,
    unresolved: bool,
}
impl WriteGuard {
    pub fn arm(&mut self) {
        self.unresolved = true;
    }
    pub fn ready(&mut self) {
        self.gate
            .state
            .store(Readiness::Ready as u8, Ordering::Release);
        self.unresolved = false;
    }
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if self.unresolved {
            self.gate
                .state
                .store(Readiness::Poisoned as u8, Ordering::Release);
        }
    }
}
