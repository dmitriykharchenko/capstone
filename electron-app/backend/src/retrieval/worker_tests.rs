use super::*;
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::oneshot;

struct Exit(Arc<AtomicUsize>);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! { static EXIT: RefCell<Option<Exit>> = const { RefCell::new(None) }; }

#[tokio::test]
async fn native_worker_preserves_typed_error_and_joins_panic() {
    for panic in [false, true] {
        let exited = Arc::new(AtomicUsize::new(0));
        let flag = exited.clone();
        let token = Cancellation::default();
        let result = blocking(token.clone(), move |_| {
            EXIT.with(|s| *s.borrow_mut() = Some(Exit(flag)));
            assert!(!panic, "synthetic native panic");
            Err::<(), _>(RetrievalError::InvalidInput)
        })
        .await;
        assert_eq!(
            result,
            Err(if panic {
                RetrievalError::Storage
            } else {
                RetrievalError::InvalidInput
            })
        );
        assert_eq!(exited.load(Ordering::SeqCst), 1);
        assert!(
            token.check().is_ok(),
            "ordinary completion must not cancel caller"
        );
    }
}

#[tokio::test]
async fn native_worker_drop_and_completion_preserve_real_sqlite_seal_outcomes() {
    for (sealed, fail, abandon) in [
        (false, false, true),
        (true, false, true),
        (true, true, true),
        (true, false, false),
        (true, true, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mutation.sqlite");
        sqlite::open(&path).unwrap().execute("PRAGMA foreign_keys=ON; CREATE TABLE parent(id PRIMARY KEY); CREATE TABLE child(id REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); INSERT INTO parent VALUES(1)").unwrap();
        let (entered, started) = oneshot::channel();
        let (release, released) = mpsc::channel();
        let (stop, stopped) = oneshot::channel();
        let (dropping, drop_started) = oneshot::channel();
        let (done, finished) = oneshot::channel();
        let token = Cancellation::default();
        let c = token.clone();
        let exited = Arc::new(AtomicUsize::new(0));
        let flag = exited.clone();
        let native_path = path.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let outcome = rt.block_on(async move {
                let future = blocking(c, move |c| {
                    EXIT.with(|s| *s.borrow_mut() = Some(Exit(flag)));
                    let db = sqlite::open(native_path)?;
                    db.execute("PRAGMA foreign_keys=ON; BEGIN")?;
                    db.execute(if fail { "INSERT INTO child VALUES(2)" } else { "INSERT INTO child VALUES(1)" })?;
                    if sealed { let _permit = c.begin_commit().map_err(|_| RetrievalError::Cancelled)?; }
                    entered.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    if !sealed { let _permit = c.begin_commit().map_err(|_| RetrievalError::Cancelled)?; }
                    db.execute("COMMIT")?;
                    Ok(())
                });
                if abandon {
                    let mut future = Box::pin(future);
                    tokio::select! { _ = &mut future => panic!("mutation completed before release"), _ = stopped => {} }
                    dropping.send(()).unwrap();
                    drop(future);
                    None
                } else { Some(future.await) }
            });
            done.send(outcome).unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .unwrap()
            .unwrap();
        if abandon {
            stop.send(()).unwrap();
            drop_started.await.unwrap();
            if !sealed {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while token.check().is_ok() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
        }
        if sealed {
            assert!(!token.cancel());
        }
        release.send(()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .unwrap()
            .unwrap();
        thread.join().unwrap();
        assert_eq!(exited.load(Ordering::SeqCst), 1);
        if !abandon {
            assert_eq!(
                outcome.unwrap(),
                if fail {
                    Err(RetrievalError::Storage)
                } else {
                    Ok(())
                }
            );
        }
        let db = sqlite::open(path).unwrap();
        let mut query = db.prepare("SELECT COUNT(*) FROM child").unwrap();
        query.next().unwrap();
        assert_eq!(query.read::<i64, _>(0).unwrap(), i64::from(sealed && !fail));
        assert_eq!(token.cancel(), !sealed);
    }
}
