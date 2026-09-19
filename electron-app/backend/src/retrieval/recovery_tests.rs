use super::recovery_fixtures as fixtures;
use super::*;
use crate::{providers::EmbeddingClient, registry::BackendFuture, runtime::Cancellation};
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

fn identity(model: &str) -> EmbeddingIdentity {
    EmbeddingIdentity {
        model: model.into(),
        dimension: 1,
    }
}
fn paper(model: &str, text: &str) -> PreparedPaper {
    PreparedPaper::new(
        "p".into(),
        identity(model),
        vec![EmbeddedChunk {
            id: "p:0".into(),
            paper_id: "p".into(),
            chunk_index: 0,
            filename: "p.pdf".into(),
            title: text.into(),
            page: 1,
            text: text.into(),
            vector: vec![0.25],
        }],
    )
    .unwrap()
}
struct NoProvider;
impl EmbeddingClient for NoProvider {
    fn embed(&self, _: Vec<String>, _: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
        panic!("provider called")
    }
}

#[tokio::test]
async fn recovery_observation_is_one_sqlite_snapshot_across_identity_revision_lexical_and_rows() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("v.sqlite");
        let mut reader = VectorStore::new(root.path(), &path, identity("old")).unwrap();
        reader.initialize(&Cancellation::default()).unwrap();
        sqlite::open(&path)
            .unwrap()
            .execute("PRAGMA journal_mode=WAL")
            .unwrap();
        let s = reader.clone();
        group
            .run_mutation("seed", move |c| async move {
                s.apply(
                    &[PreparedOperation::ReplacePaper(paper("old", "Before"))],
                    None,
                    &c,
                )
                .map_err(Into::into)
            })
            .await
            .unwrap();
        let before = reader
            .observe_paper("p", &group.preparation_stop())
            .unwrap();
        let (start, started) = tokio::sync::oneshot::channel();
        let start = Mutex::new(Some(start));
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        reader.snapshot_hook = Some(Arc::new(move |db, _| {
            // BEGIN alone has not pinned SQLite's snapshot; a real read does.
            db.execute("SELECT revision FROM retrieval_meta").unwrap();
            start.lock().unwrap().take().unwrap().send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        }));
        let c = group.preparation_stop();
        let worker = std::thread::spawn(move || reader.observe_paper("p", &c));
        started.await.unwrap();
        let writer = VectorStore::new(root.path(), &path, identity("new")).unwrap();
        let s = writer.clone();
        group
            .run_mutation("replace identity", move |c| async move {
                s.apply(
                    &[
                        PreparedOperation::Reset(PreparedReset::new(identity("new")).unwrap()),
                        PreparedOperation::ReplacePaper(paper("new", "After!")),
                        PreparedOperation::RebuildLexical,
                    ],
                    Some(1),
                    &c,
                )
                .map_err(Into::into)
            })
            .await
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), before);
        let after = writer
            .observe_paper("p", &group.preparation_stop())
            .unwrap();
        assert_eq!(before.revision, 1);
        assert_eq!(before.lexical, LexicalStatus::Stale);
        assert_eq!(before.artifact.identity().model, "old");
        assert_eq!(after.revision, 2);
        assert_eq!(after.lexical, LexicalStatus::Ready);
        assert_eq!(after.artifact.identity().model, "new");
        assert_ne!(before.artifact.digest(), after.artifact.digest());
    })
    .await;
}

#[tokio::test]
async fn recovery_store_corruption_is_not_absence_and_reads_do_not_repair() {
    fixtures::with_group(|group| async move {
        for corrupt in [
            "UPDATE chunks SET payload='{}'",
            "UPDATE chunks SET paper_id='wrong'",
            "PRAGMA user_version=99",
            "INSERT INTO lexical VALUES(1,'broken')",
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("v.sqlite");
            let store = VectorStore::new(root.path(), &path, identity("old")).unwrap();
            store.initialize(&Cancellation::default()).unwrap();
            let s = store.clone();
            group
                .run_mutation("seed", move |c| async move {
                    s.apply(
                        &[PreparedOperation::ReplacePaper(paper("old", "Before"))],
                        None,
                        &c,
                    )
                    .map_err(Into::into)
                })
                .await
                .unwrap();
            let p = path.clone();
            group
                .run_mutation("corrupt fixture", move |c| async move {
                    let db = sqlite::open(p).unwrap();
                    db.execute("BEGIN IMMEDIATE").unwrap();
                    db.execute(corrupt).unwrap();
                    let _permit = c.begin_commit()?;
                    db.execute("COMMIT").unwrap();
                    Ok(())
                })
                .await
                .unwrap();
            let bytes = std::fs::read(&path).unwrap();
            for _ in 0..2 {
                let result = store.observe_paper("p", &group.preparation_stop());
                if corrupt.starts_with("INSERT") {
                    assert_eq!(result.unwrap().lexical, LexicalStatus::Stale);
                } else {
                    assert_eq!(result.unwrap_err(), RetrievalError::InvalidStore);
                }
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
        }
    })
    .await;
}

struct Exit(Arc<AtomicBool>);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
thread_local! { static EXIT: RefCell<Option<Exit>> = const { RefCell::new(None) }; }

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_observation_drop_and_cancel_join_real_sqlite_reader() {
    for abort in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("v.sqlite");
        let mut store = VectorStore::new(root.path(), &path, identity("old")).unwrap();
        store.initialize(&Cancellation::default()).unwrap();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = exited.clone();
        let (start, started) = tokio::sync::oneshot::channel();
        let start = Mutex::new(Some(start));
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        store.snapshot_hook = Some(Arc::new(move |db, _| {
            EXIT.with(|e| *e.borrow_mut() = Some(Exit(flag.clone())));
            db.execute("SELECT revision FROM retrieval_meta").unwrap();
            start.lock().unwrap().take().unwrap().send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        }));
        let c = Cancellation::default();
        let token = c.clone();
        let service = NativeRetriever::new(store, Arc::new(NoProvider), None);
        let task = tokio::spawn(async move { service.observe_paper("p".into(), token).await });
        started.await.unwrap();
        if abort {
            task.abort();
        } else {
            assert!(c.cancel());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while c.check().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        assert!(!exited.load(Ordering::SeqCst));
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap();
        if abort {
            assert!(result.unwrap_err().is_cancelled());
        } else {
            assert_eq!(result.unwrap().unwrap_err(), RetrievalError::Cancelled);
        }
        assert!(exited.load(Ordering::SeqCst));
        sqlite::open(path)
            .unwrap()
            .execute("BEGIN EXCLUSIVE; ROLLBACK")
            .unwrap();
    }
}
