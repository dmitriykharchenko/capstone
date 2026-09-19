use super::*;
use crate::retrieval::recovery_fixtures as fixtures;
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

fn replacement(title: &str) -> GraphOperation {
    GraphOperation::Replace(
        PreparedPaper::new("p", title, &[title.into()], &[title.into()]).unwrap(),
    )
}

#[tokio::test]
async fn recovery_graph_reads_reject_corrupt_identity_normalization_and_dangling_edges_without_repair()
 {
    fixtures::with_group(|group| async move {
        for script in [
            "?[author_id, name, normalized_name] := *author{author_id}, name = 'Before', normalized_name = 'wrong' :put author {author_id => name, normalized_name}",
            "?[author_id, name, normalized_name] := *author{author_id}, name = 'Other', normalized_name = 'other' :put author {author_id => name, normalized_name}",
            "?[author_id] := *author{author_id} :rm author {author_id}",
            "?[paper_id] <- [['p']] :rm paper {paper_id}",
            "?[paper_id, author_id] := *authored_by{paper_id, author_id} :rm authored_by {paper_id, author_id}",
            "?[paper_id, title] <- [['p', ' Before ']] :put paper {paper_id => title}",
        ] {
            let root=tempfile::tempdir().unwrap(); let path=root.path().join("g.sqlite");
            let store=CozoGraphStore::open(root.path().into(),path.clone(),Cancellation::default()).await.unwrap();
            let s=store.clone();
            group.run_mutation("seed",move |c| async move { s.apply(vec![replacement("Before")],c).await.map_err(Into::into) }).await.unwrap();
            let s=store.clone();
            group.run_mutation("corrupt fixture",move |c| async move {
                let guard=s.inner.lock().unwrap(); let tx=Transaction::new(&guard.as_ref().unwrap().db,true);
                tx.run(script,BTreeMap::new(),&c).unwrap(); tx.commit(&c).map_err(Into::into)
            }).await.unwrap();
            let bytes=fs::read(&path).unwrap();
            for _ in 0..2 {
                assert_eq!(store.observe_paper("p".into(),group.preparation_stop()).await.unwrap_err(),GraphError::InvalidSchema);
                let s=store.clone();
                let result=group.run_mutation("refuse corrupt restore",move |c|async move {
                    Ok(s.restore_paper(PaperRecoveryArtifact::absent("p".into()).unwrap(),c).await)
                }).await.unwrap();
                assert_eq!(result,Err(GraphError::InvalidSchema));
                assert_eq!(fs::read(&path).unwrap(),bytes);
            }
            store.close().await.unwrap();
        }
    }).await;
}

#[tokio::test]
async fn recovery_graph_observation_serializes_title_nodes_and_edges_with_writer() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("g.sqlite");
        let store = CozoGraphStore::open(root.path().into(), path, Cancellation::default())
            .await
            .unwrap();
        let s = store.clone();
        group
            .run_mutation("seed", move |c| async move {
                s.apply(vec![replacement("Before")], c)
                    .await
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        let before = store
            .observe_paper("p".into(), group.preparation_stop())
            .await
            .unwrap();
        let (start, started) = tokio::sync::oneshot::channel();
        let start = Mutex::new(Some(start));
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let mut reader = store.clone();
        reader.snapshot_hook = Some(Arc::new(move |_| {
            start.lock().unwrap().take().unwrap().send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        }));
        let c = group.preparation_stop();
        let read = tokio::spawn(async move { reader.observe_paper("p".into(), c).await });
        started.await.unwrap();
        assert!(
            store.inner.try_lock().is_err(),
            "observation owns store lock"
        );
        let s = store.clone();
        let g = group.clone();
        let (admit, admitted) = tokio::sync::oneshot::channel();
        let write = tokio::spawn(async move {
            g.run_mutation("concurrent write", move |c| async move {
                admit.send(()).unwrap();
                s.apply(vec![replacement("After!")], c)
                    .await
                    .map_err(Into::into)
            })
            .await
        });
        admitted.await.unwrap();
        assert!(!write.is_finished());
        release.send(()).unwrap();
        assert_eq!(read.await.unwrap().unwrap(), before);
        write.await.unwrap().unwrap();
        let after = store
            .observe_paper("p".into(), group.preparation_stop())
            .await
            .unwrap();
        assert_ne!(before.digest(), after.digest());
        let p = after.paper().unwrap();
        assert_eq!(p.title, "After!");
        assert_eq!(p.authors[0].name, "After!");
        assert_eq!(p.topics[0].name, "After!");
        store.close().await.unwrap();
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
async fn recovery_graph_observation_drop_and_cancel_join_native_transaction() {
    for (restore, after_stage) in [(false, false), (true, false), (true, true)] {
        for abort in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("g.sqlite");
            let mut store =
                CozoGraphStore::open(root.path().into(), path.clone(), Cancellation::default())
                    .await
                    .unwrap();
            let exited = Arc::new(AtomicBool::new(false));
            let flag = exited.clone();
            let (start, started) = tokio::sync::oneshot::channel();
            let start = Mutex::new(Some(start));
            let (release, released) = mpsc::channel();
            let released = Mutex::new(released);
            let hook: Arc<dyn Fn(&Cancellation) + Send + Sync> = Arc::new(move |_| {
                EXIT.with(|e| *e.borrow_mut() = Some(Exit(flag.clone())));
                start.lock().unwrap().take().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            });
            if restore {
                store.restore_hook = Some(Arc::new(move |staged, c| {
                    if staged == after_stage {
                        hook(c);
                    }
                }));
            } else {
                store.snapshot_hook = Some(hook);
            }
            let c = Cancellation::default();
            let token = c.clone();
            let s = store.clone();
            let bytes = fs::read(&path).unwrap();
            let task = tokio::spawn(async move {
                if restore {
                    let p = PreparedPaper::new("p", "Title", &["Alice".into()], &["RAG".into()])
                        .unwrap();
                    s.restore_paper(PaperRecoveryArtifact::from_prepared(&p).unwrap(), token)
                        .await
                } else {
                    s.observe_paper("p".into(), token).await.map(|_| ())
                }
            });
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
                assert_eq!(result.unwrap().unwrap_err(), GraphError::Cancelled);
            }
            assert!(exited.load(Ordering::SeqCst));
            assert!(store.inner.try_lock().is_ok());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            sqlite::open(path)
                .unwrap()
                .execute("BEGIN EXCLUSIVE; ROLLBACK")
                .unwrap();
            store.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn recovery_graph_restore_rejects_malformed_other_paper_incident_edges() {
    fixtures::with_group(|group| async move {
        for script in [
            "?[paper_id] <- [['q']] :rm paper {paper_id}",
            "?[paper_id, title] <- [['q', ' Other ']] :put paper {paper_id => title}",
            "?[topic_id, name, normalized_name] := *topic{topic_id}, name = 'Shared', normalized_name = 'wrong' :put topic {topic_id => name, normalized_name}",
            "?[author_id] := *author{author_id} :rm author {author_id}",
            "?[topic_id] := *topic{topic_id} :rm topic {topic_id}",
        ] {
            let root=tempfile::tempdir().unwrap(); let path=root.path().join("g.sqlite");
            let store=CozoGraphStore::open(root.path().into(),path.clone(),Cancellation::default()).await.unwrap();
            let s=store.clone();
            group.run_mutation("seed other",move |c|async move {
                let p=PreparedPaper::new("q","Other",&["Shared".into()],&["Shared".into()]).unwrap();
                s.apply(vec![GraphOperation::Replace(p)],c).await.map_err(Into::into)
            }).await.unwrap();
            let s=store.clone();
            group.run_mutation("corrupt other",move |c|async move {
                let guard=s.inner.lock().unwrap();let tx=Transaction::new(&guard.as_ref().unwrap().db,true);
                tx.run(script,BTreeMap::new(),&c).unwrap();tx.commit(&c).map_err(Into::into)
            }).await.unwrap();
            let bytes=fs::read(&path).unwrap();
            let s=store.clone();
            let desired=PaperRecoveryArtifact::from_prepared(&PreparedPaper::new("p","Title",&["Shared".into()],&["Shared".into()]).unwrap()).unwrap();
            let result=group.run_mutation("refuse other corruption",move |c|async move {Ok(s.restore_paper(desired,c).await)}).await.unwrap();
            assert_eq!(result,Err(GraphError::InvalidSchema));
            assert_eq!(fs::read(&path).unwrap(),bytes);
            store.close().await.unwrap();
        }
    }).await;
}

#[tokio::test]
async fn recovery_graph_restore_guard_and_write_are_serialized_with_competing_request() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("g.sqlite");
        let store = CozoGraphStore::open(root.path().into(), path, Cancellation::default())
            .await
            .unwrap();
        let desired = PaperRecoveryArtifact::from_prepared(
            &PreparedPaper::new("p", "Title", &["Alice".into()], &["RAG".into()]).unwrap(),
        )
        .unwrap();
        let (start, started) = tokio::sync::oneshot::channel();
        let start = Mutex::new(Some(start));
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let mut restoring = store.clone();
        restoring.restore_hook = Some(Arc::new(move |staged, _| {
            if !staged {
                start.lock().unwrap().take().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
        }));
        let artifact = desired.clone();
        let g = group.clone();
        let restore = tokio::spawn(async move {
            g.run_mutation("guarded restore", move |c| async move {
                restoring
                    .restore_paper(artifact, c)
                    .await
                    .map_err(Into::into)
            })
            .await
        });
        started.await.unwrap();
        assert!(store.inner.try_lock().is_err());
        let (enter, entered) = tokio::sync::oneshot::channel();
        let s = store.clone();
        let expected = desired.clone();
        let writer = tokio::spawn(fixtures::with_group(move |other| async move {
            enter.send(()).unwrap();
            assert_eq!(
                s.observe_paper("p".into(), other.preparation_stop())
                    .await
                    .unwrap(),
                expected
            );
            other
                .run_mutation("ordinary competing writer", move |c| async move {
                    s.apply(
                        vec![GraphOperation::Replace(
                            PreparedPaper::new("q", "Other", &["ALICE".into()], &["rag".into()])
                                .unwrap(),
                        )],
                        c,
                    )
                    .await
                    .map_err(Into::into)
                })
                .await
                .unwrap();
        }));
        entered.await.unwrap();
        assert!(!writer.is_finished());
        release.send(()).unwrap();
        restore.await.unwrap().unwrap();
        writer.await.unwrap();
        let actual = store
            .observe_paper("p".into(), group.preparation_stop())
            .await
            .unwrap();
        assert_ne!(actual, desired);
        let s = store.clone();
        let result = group
            .run_mutation("recheck latest shared state", move |c| async move {
                Ok(s.restore_paper(desired, c).await)
            })
            .await
            .unwrap();
        assert_eq!(result, Err(GraphError::SharedNodeConflict));
        assert_eq!(
            store
                .observe_paper("p".into(), group.preparation_stop())
                .await
                .unwrap(),
            actual
        );
        store.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn recovery_graph_isolated_restore_preserves_unrelated_orphans() {
    fixtures::with_group(|group| async move {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("g.sqlite");
        let store=CozoGraphStore::open(root.path().into(),path,Cancellation::default()).await.unwrap();
        let s=store.clone();
        group.run_mutation("seed isolated",move |c|async move {s.apply(vec![replacement("Before")],c).await.map_err(Into::into)}).await.unwrap();
        let s=store.clone();
        group.run_mutation("seed orphan fixture",move |c|async move {
            let guard=s.inner.lock().unwrap();let tx=Transaction::new(&guard.as_ref().unwrap().db,true);
            let author=stable_node_id(NodeKind::Author,"Orphan").unwrap();
            tx.run("?[author_id, name, normalized_name] <- [[$id, 'Orphan', 'orphan']] :put author {author_id => name, normalized_name}",params(json!({"id":author})),&c).unwrap();
            tx.commit(&c).map_err(Into::into)
        }).await.unwrap();
        let target=PaperRecoveryArtifact::from_prepared(&PreparedPaper::new("p","After",&["BEFORE".into()],&["BEFORE".into()]).unwrap()).unwrap();
        let s=store.clone();let desired=target.clone();
        group.run_mutation("restore isolated labels",move |c|async move {s.restore_paper(desired,c).await.map_err(Into::into)}).await.unwrap();
        assert_eq!(store.observe_paper("p".into(),group.preparation_stop()).await.unwrap(),target);
        assert_eq!(store.status(group.preparation_stop()).await.unwrap().authors,2);
        store.close().await.unwrap();
    }).await;
}
