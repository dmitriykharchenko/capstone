use super::{IndexError, IndexResult};
use crate::runtime::Cancellation;

/// Native work is never detached, including future-drop and panic. Poll/drop on
/// a blocking-capable request runner; no timeout implies rollback or worker exit.
pub(super) async fn joined<T: Send + 'static>(
    c: Cancellation,
    work: impl FnOnce(Cancellation) -> IndexResult<T> + Send + 'static,
) -> IndexResult<T> {
    struct Worker(Option<std::thread::JoinHandle<()>>, Cancellation);
    impl Drop for Worker {
        fn drop(&mut self) {
            if let Some(t) = self.0.take() {
                self.1.cancel();
                let _ = t.join();
            }
        }
    }
    c.check()?;
    let (send, receive) = tokio::sync::oneshot::channel();
    let token = c.clone();
    let thread = std::thread::Builder::new()
        .name("index-journal".into())
        .spawn(move || {
            let _ = send.send(work(token));
        })?;
    let mut worker = Worker(Some(thread), c);
    let result = receive.await.map_err(|_| IndexError::Storage);
    worker
        .0
        .take()
        .unwrap()
        .join()
        .map_err(|_| IndexError::Storage)?;
    result?
}
