use super::{GraphError, GraphResult};
use crate::{protocol::ErrorCode, runtime::Cancellation};
use cozo::{DataValue, DbInstance, MultiTransaction, NamedRows, TransactionPayload};
use std::collections::BTreeMap;

pub(super) fn check(c: &Cancellation) -> GraphResult<()> {
    c.check().map_err(|_| GraphError::Cancelled)
}

pub(super) struct Transaction(Option<MultiTransaction>);
impl Transaction {
    pub fn new(db: &DbInstance, write: bool) -> Self {
        Self(Some(db.multi_transaction(write)))
    }
    pub fn run(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
        c: &Cancellation,
    ) -> GraphResult<NamedRows> {
        check(c)?;
        let result = self
            .0
            .as_ref()
            .ok_or(GraphError::Closed)?
            .run_script(script, params)
            .map_err(|_| GraphError::Storage)?;
        check(c)?;
        Ok(result)
    }
    pub fn commit(self, c: &Cancellation) -> GraphResult<()> {
        self.commit_observed(c, || {})
    }
    pub(super) fn commit_observed(
        mut self,
        c: &Cancellation,
        after_seal: impl FnOnce(),
    ) -> GraphResult<()> {
        let _permit = c.begin_commit().map_err(|e| {
            if e == ErrorCode::Cancelled {
                GraphError::Cancelled
            } else {
                GraphError::AlreadySealed
            }
        })?;
        after_seal();
        self.finish(TransactionPayload::Commit)
    }
    pub fn abort(mut self) -> GraphResult<()> {
        self.finish(TransactionPayload::Abort)
    }
    // Only for an unpublished initialization file, never a live graph mutation.
    pub fn commit_staging(mut self) -> GraphResult<()> {
        self.finish(TransactionPayload::Commit)
    }
    fn finish(&mut self, payload: TransactionPayload) -> GraphResult<()> {
        let tx = self.0.take().ok_or(GraphError::Closed)?;
        let sent = tx.sender.send(payload).is_ok();
        drop(tx.sender);
        // Unlike MultiTransaction::commit(), inspect the nested database result.
        let result = tx
            .receiver
            .recv()
            .map_err(|_| GraphError::Storage)
            .and_then(|r| r.map(|_| ()).map_err(|_| GraphError::Storage));
        // Abort acknowledgment precedes rollback in Cozo. Wait for worker teardown,
        // not just that acknowledgment, before releasing the outer store mutex.
        for _ in tx.receiver {}
        if sent {
            result
        } else {
            Err(GraphError::Storage)
        }
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        if self.0.is_some() {
            let _ = self.finish(TransactionPayload::Abort);
        }
    }
}

/// A dropped async future cancels and joins its native worker instead of silently
/// detaching it. Drop may synchronously wait for the current native statement;
/// cancellation is cooperative between statements, not syscall preemption.
pub(super) async fn joined<T: Send + 'static>(
    c: Cancellation,
    f: impl FnOnce(Cancellation) -> GraphResult<T> + Send + 'static,
) -> GraphResult<T> {
    struct Worker {
        thread: Option<std::thread::JoinHandle<()>>,
        cancellation: Cancellation,
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                self.cancellation.cancel();
                let _ = thread.join();
            }
        }
    }
    check(&c)?;
    let (send, receive) = tokio::sync::oneshot::channel();
    let token = c.clone();
    let thread = std::thread::Builder::new()
        .name("paper-graph".into())
        .spawn(move || {
            let _ = send.send(f(token));
        })
        .map_err(|_| GraphError::Storage)?;
    let mut worker = Worker {
        thread: Some(thread),
        cancellation: c,
    };
    let result = receive.await.map_err(|_| GraphError::Storage);
    worker
        .thread
        .take()
        .unwrap()
        .join()
        .map_err(|_| GraphError::Storage)?;
    result?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn graph_transaction_channel_disconnect_is_an_error() {
        let db = DbInstance::new("mem", "", "").unwrap();
        let tx = Transaction::new(&db, true);
        let raw = tx.0.as_ref().unwrap();
        raw.sender.send(TransactionPayload::Abort).unwrap();
        raw.receiver.recv().unwrap().unwrap();
        assert!(raw.receiver.recv().is_err());
        let c = Cancellation::default();
        assert_eq!(tx.commit(&c), Err(GraphError::Storage));
        assert!(!c.cancel());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_dropped_future_cancels_and_joins_native_worker() {
        let (start, started) = tokio::sync::oneshot::channel();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let c = Cancellation::default();
        let task = tokio::spawn(joined(c.clone(), move |token| {
            start.send(()).unwrap();
            while token.check().is_ok() {
                std::thread::yield_now();
            }
            flag.store(true, Ordering::Release);
            Err::<(), _>(GraphError::Cancelled)
        }));
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(done.load(Ordering::Acquire));
        assert_eq!(check(&c), Err(GraphError::Cancelled));
    }
}
