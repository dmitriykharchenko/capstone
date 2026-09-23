use super::snapshot_tests::fixtures;
use super::*;
use crate::{registry::Registry, runtime::serve};
use futures_util::FutureExt;
use serde_json::json;
use std::{
    cell::RefCell,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Exit(Arc<AtomicBool>);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
thread_local! { static EXIT: RefCell<Option<Exit>> = const { RefCell::new(None) }; }

#[tokio::test]
async fn preview_worker_joins_normal_error_and_panic_before_returning() {
    for panic in [false, true] {
        let exited = Arc::new(AtomicBool::new(false));
        let flag = exited.clone();
        let result = super::super::preview::owned(Cancellation::default(), move |_| {
            EXIT.with(|s| *s.borrow_mut() = Some(Exit(flag)));
            assert!(!panic, "synthetic preview panic");
            Err::<(), _>(ErrorCode::PdfInvalid)
        })
        .await;
        assert_eq!(
            result,
            Err(if panic {
                ErrorCode::PdfClosed
            } else {
                ErrorCode::PdfInvalid
            })
        );
        assert!(exited.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn dropping_preview_future_cancels_and_joins_native_thread() {
    let token = Cancellation::default();
    let c = token.clone();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let flag = exited.clone();
    let runner = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut future = Box::pin(super::super::preview::owned(c, move |_| {
                EXIT.with(|s| *s.borrow_mut() = Some(Exit(flag)));
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            }));
            tokio::select! { _ = &mut future => panic!("early completion"), _ = stopped => {} }
            drop(future);
        });
    });
    started.await.unwrap();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while token.check().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!runner.is_finished());
    release.send(()).unwrap();
    runner.await.unwrap();
    assert!(exited.load(Ordering::SeqCst));
}

#[tokio::test]
async fn actual_read_worker_is_joined_before_cancel_terminal_or_eof_shutdown() {
    for eof in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let bytes = fixtures::pdf(vec![fixtures::Page::Blank]);
        std::fs::write(temp.path().join("p.pdf"), &bytes).unwrap();
        let mut repository = NativePaperRepository::new(temp.path()).unwrap();
        let (entered, started) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let exited = Arc::new(AtomicBool::new(false));
        let flag = exited.clone();
        repository.before_preview = Some(Arc::new(move || {
            EXIT.with(|s| *s.borrow_mut() = Some(Exit(flag.clone())));
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }));
        let mut registry = Registry::foundation();
        registry
            .install(
                "read_pdf",
                "papers",
                Arc::new(PaperCommandHandler::new(repository, PaperCommand::ReadPdf)),
            )
            .unwrap();
        let (client, server) = tokio::io::duplex(4096);
        let (input, output) = tokio::io::split(server);
        let task = tokio::spawn(serve(input, output, registry));
        let (reader, mut writer) = tokio::io::split(client);
        let mut lines = BufReader::new(reader).lines();
        writer.write_all(b"{\"v\":1,\"type\":\"request\",\"id\":\"p\",\"command\":\"read_pdf\",\"params\":{\"paper_id\":\"p\"}}\n").await.unwrap();
        started.await.unwrap();
        if eof {
            writer.shutdown().await.unwrap();
        } else {
            writer
                .write_all(b"{\"v\":1,\"type\":\"cancel\",\"id\":\"p\"}\n")
                .await
                .unwrap();
            let ack: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(ack["accepted"], true);
            writer.write_all(b"{\"v\":1,\"type\":\"request\",\"id\":\"h\",\"command\":\"health\",\"params\":{}}\n").await.unwrap();
            let health: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(health["id"], "h");
        }
        assert!(!exited.load(Ordering::SeqCst));
        assert!(!task.is_finished());
        assert!(lines.next_line().now_or_never().is_none());
        release.send(()).unwrap();
        let terminal: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(terminal, json!({"v":1,"type":"cancelled","id":"p","seq":0}));
        assert!(exited.load(Ordering::SeqCst));
        writer.shutdown().await.unwrap();
        assert!(lines.next_line().await.unwrap().is_none());
        task.await.unwrap().unwrap();
        assert_eq!(std::fs::read(temp.path().join("p.pdf")).unwrap(), bytes);
    }
}
