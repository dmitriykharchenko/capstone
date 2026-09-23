//! Preview-only descriptor leases. No document capture, hashing or text parser.
//! Identity is an observation, not protection against an adversarial filesystem.
use super::{SourceIdentity, safe_root::SafeRoot};
use crate::runtime::Cancellation;
use crate::{domain::PdfInfo, protocol::ErrorCode, registry::BackendResult};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub const MAX_PREVIEW_SESSIONS: usize = 4;
pub const MAX_PREVIEW_RANGE: u64 = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(super) struct Sessions(Mutex<HashMap<String, Arc<Session>>>);
struct Data {
    file: Option<File>,
    touched: Instant,
    error: Option<ErrorCode>,
}
struct Session {
    root: Arc<SafeRoot>,
    paper_id: String,
    identity: SourceIdentity,
    cancellation: Cancellation,
    data: Mutex<Data>,
    changed: Notify,
}
pub(super) struct Lease {
    id: String,
    sessions: Arc<Sessions>,
    session: Arc<Session>,
}

impl Session {
    fn close(&self, code: ErrorCode) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.file = None;
        data.error.get_or_insert(code);
        self.changed.notify_one();
    }
    fn identity(&self, file: &File) -> BackendResult<()> {
        let observed = || {
            let pinned = SourceIdentity::from_metadata(&file.metadata().ok()?).ok()?;
            let current = SourceIdentity::from_metadata(
                &self.root.open(&self.paper_id).ok()?.metadata().ok()?,
            )
            .ok()?;
            Some(pinned == self.identity && current == self.identity)
        };
        if observed() == Some(true) {
            Ok(())
        } else {
            Err(ErrorCode::PdfChanged)
        }
    }
}
impl Sessions {
    pub(super) fn open(
        self: &Arc<Self>,
        root: Arc<SafeRoot>,
        paper_id: String,
        cancellation: Cancellation,
    ) -> BackendResult<Lease> {
        cancellation.check()?;
        // Reserve under the same lock as admission; at most four live leases even
        // when simultaneous opens race. Only bounded header IO happens here.
        let mut sessions = self.0.lock().map_err(|_| ErrorCode::PdfClosed)?;
        if sessions.len() >= MAX_PREVIEW_SESSIONS {
            return Err(ErrorCode::Busy);
        }
        let mut file = root.open(&paper_id).map_err(|_| ErrorCode::PdfInvalid)?;
        let identity =
            SourceIdentity::from_metadata(&file.metadata().map_err(|_| ErrorCode::PdfInvalid)?)
                .map_err(|_| ErrorCode::PdfInvalid)?;
        let mut header = vec![0; identity.size_bytes.min(1024) as usize];
        file.read_exact(&mut header)
            .map_err(|_| ErrorCode::PdfInvalid)?;
        if !header.windows(5).any(|b| b == b"%PDF-") {
            return Err(ErrorCode::PdfInvalid);
        }
        // PDF.js owns structural parsing/password handling; the native preview
        // never invokes the whole-file extraction parser.
        let session = Arc::new(Session {
            root,
            paper_id,
            identity,
            cancellation: cancellation.clone(),
            data: Mutex::new(Data {
                file: Some(file),
                touched: Instant::now(),
                error: None,
            }),
            changed: Notify::new(),
        });
        session.identity(
            session
                .data
                .lock()
                .map_err(|_| ErrorCode::PdfClosed)?
                .file
                .as_ref()
                .unwrap(),
        )?;
        cancellation.check()?;
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(id.clone(), session.clone());
        Ok(Lease {
            id,
            sessions: self.clone(),
            session,
        })
    }
    pub(super) fn close(&self, id: &str, code: ErrorCode) {
        if let Ok(sessions) = self.0.lock()
            && let Some(session) = sessions.get(id)
        {
            session.close(code);
        }
    }
    pub(super) fn read(
        &self,
        id: &str,
        begin: u64,
        end: u64,
        cancellation: &Cancellation,
    ) -> BackendResult<Vec<u8>> {
        let session = self
            .0
            .lock()
            .map_err(|_| ErrorCode::PdfClosed)?
            .get(id)
            .cloned()
            .ok_or(ErrorCode::PdfClosed)?;
        // Serialize within the existing bounded native request window. A lease
        // timer briefly takes this mutex too; that must not reject valid reads.
        let mut data = session.data.lock().map_err(|_| ErrorCode::PdfClosed)?;
        let result = (|| {
            cancellation.check()?;
            session
                .cancellation
                .check()
                .map_err(|_| ErrorCode::PdfClosed)?;
            if data.touched.elapsed() >= IDLE_TIMEOUT {
                return Err(ErrorCode::PdfClosed);
            }
            let closed = data.error.unwrap_or(ErrorCode::PdfClosed);
            let file = data.file.as_mut().ok_or(closed)?;
            if begin >= end || end > session.identity.size_bytes || end - begin > MAX_PREVIEW_RANGE
            {
                return Err(ErrorCode::PdfInvalid);
            }
            session.identity(file)?;
            file.seek(SeekFrom::Start(begin))
                .map_err(|_| ErrorCode::PdfChanged)?;
            let mut bytes = vec![0; (end - begin) as usize];
            file.read_exact(&mut bytes)
                .map_err(|_| ErrorCode::PdfChanged)?;
            session.identity(file)?;
            cancellation.check()?;
            session
                .cancellation
                .check()
                .map_err(|_| ErrorCode::PdfClosed)?;
            Ok(bytes)
        })();
        if let Err(code) = result {
            data.file = None;
            data.error.get_or_insert(code);
        } else {
            data.touched = Instant::now();
        }
        session.changed.notify_one();
        result
    }
}
impl Lease {
    pub(super) fn info(&self) -> PdfInfo {
        PdfInfo {
            id: self.session.paper_id.clone(),
            filename: format!("{}.pdf", self.session.paper_id),
            mime_type: "application/pdf".into(),
            total_bytes: self.session.identity.size_bytes,
            preview_id: self.id.clone(),
        }
    }
    pub(super) async fn wait(&self) -> ErrorCode {
        loop {
            let remaining = {
                let data = self.session.data.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(code) = data.error {
                    return code;
                }
                IDLE_TIMEOUT.saturating_sub(data.touched.elapsed())
            };
            if remaining.is_zero() {
                self.session.close(ErrorCode::PdfClosed);
                return ErrorCode::PdfClosed;
            }
            let _ = tokio::time::timeout(remaining, self.session.changed.notified()).await;
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.session.close(ErrorCode::PdfClosed);
        self.sessions
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// A request-owned native worker, joined on success, error, panic AND Drop.
/// Drop may wait for bounded native IO. The runtime already isolates
/// handler polling/drop on its blocking-capable request runner.
pub(super) async fn owned<T: Send + 'static>(
    cancellation: Cancellation,
    work: impl FnOnce(Cancellation) -> BackendResult<T> + Send + 'static,
) -> BackendResult<T> {
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
    cancellation.check()?;
    let (send, receive) = tokio::sync::oneshot::channel();
    let token = cancellation.clone();
    let thread = std::thread::Builder::new()
        .name("native-pdf-preview".into())
        .spawn(move || {
            let _ = send.send(work(token));
        })
        .map_err(|_| ErrorCode::PdfClosed)?;
    let mut worker = Worker {
        thread: Some(thread),
        cancellation,
    };
    let result = receive.await.map_err(|_| ErrorCode::PdfClosed);
    worker
        .thread
        .take()
        .unwrap()
        .join()
        .map_err(|_| ErrorCode::PdfClosed)?;
    result?
}

#[cfg(test)]
#[path = "preview_session_tests.rs"]
mod tests;
