//! Structural preflight, not text extraction or proof of Chromium rendering.
//! The input cap does NOT bound parser decompression memory or execution time.
use super::{PaperError, PaperResult, extraction::check};
use crate::runtime::Cancellation;
use pdf_extract::Document;

pub(super) fn validate(bytes: &[u8], cancellation: &Cancellation) -> PaperResult<()> {
    check(cancellation)?;
    let result =
        std::panic::catch_unwind(|| structure(bytes)).unwrap_or(Err(PaperError::CorruptPdf));
    check(cancellation)?;
    result
}

fn structure(bytes: &[u8]) -> PaperResult<()> {
    let doc = Document::load_mem(bytes).map_err(|_| PaperError::CorruptPdf)?;
    let validate = || -> Result<(), pdf_extract::Error> {
        if doc.trailer.has(b"Encrypt") {
            // Without a password the parser leaves encrypted objects unloaded.
            // Validate the parsed encryption dictionary and root xref, NOT the
            // inaccessible page tree. Chromium owns authentication and rendering.
            let encryption = doc.get_encrypted()?;
            let filter = encryption.get(b"Filter")?.as_name()?;
            if filter == b"Standard" {
                // Check the public dictionary shape, not authentication. This
                // prevents a bare /Encrypt marker from bypassing validation.
                encryption.get(b"R")?.as_i64()?;
                encryption.get(b"P")?.as_i64()?;
                encryption.get(b"O")?.as_str()?;
                encryption.get(b"U")?.as_str()?;
            }
            let root = doc.trailer.get(b"Root")?.as_reference()?;
            if !doc
                .reference_table
                .get(root.0)
                .is_some_and(|entry| entry.is_normal() || entry.is_compressed())
            {
                return Err(pdf_extract::Error::ObjectNotFound(root));
            }
        } else {
            let catalog = doc.catalog()?;
            let pages = doc.get_dict_in_dict(catalog, b"Pages")?;
            let declared = pages.get(b"Count")?.as_i64()?;
            let actual = doc.get_pages();
            if declared < 0 || actual.len() as i64 != declared {
                return Err(pdf_extract::Error::ObjectNotFound((0, 0)));
            }
            for id in actual.values() {
                doc.get_object(*id)?.as_dict()?;
            }
        }
        Ok(())
    };
    validate().map_err(|_| PaperError::CorruptPdf)
}

/// A request-owned native worker, joined on success, error, panic AND Drop.
/// Drop may wait for noninterruptible parsing. The runtime already isolates
/// handler polling/drop on its blocking-capable request runner.
pub(super) async fn owned<T: Send + 'static>(
    cancellation: Cancellation,
    work: impl FnOnce(Cancellation) -> PaperResult<T> + Send + 'static,
) -> PaperResult<T> {
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
    check(&cancellation)?;
    let (send, receive) = tokio::sync::oneshot::channel();
    let token = cancellation.clone();
    let thread = std::thread::Builder::new()
        .name("native-pdf-preview".into())
        .spawn(move || {
            let _ = send.send(work(token));
        })
        .map_err(|_| PaperError::Io)?;
    let mut worker = Worker {
        thread: Some(thread),
        cancellation,
    };
    let result = receive.await.map_err(|_| PaperError::Io);
    worker
        .thread
        .take()
        .unwrap()
        .join()
        .map_err(|_| PaperError::Io)?;
    result?
}
