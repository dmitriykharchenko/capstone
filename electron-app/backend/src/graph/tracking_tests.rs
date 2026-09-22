use super::*;
use crate::{retrieval::recovery_fixtures as fixtures, runtime::OperationGroup};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

async fn open(root: &Path) -> CozoGraphStore {
    CozoGraphStore::open(
        root.into(),
        root.join("graph.sqlite3"),
        Cancellation::default(),
    )
    .await
    .unwrap()
}

fn replace(id: &str, label: &str) -> GraphOperation {
    GraphOperation::Replace(
        PreparedPaper::new(id, label, &[label.into()], &[label.into()]).unwrap(),
    )
}

async fn apply(
    group: &OperationGroup,
    store: &CozoGraphStore,
    ops: Vec<GraphOperation>,
) -> GraphResult<()> {
    let s = store.clone();
    group
        .run_mutation("graph tracking test", move |c| async move {
            Ok(s.apply(ops, c).await)
        })
        .await
        .unwrap()
}

async fn raw_write(group: &OperationGroup, store: &CozoGraphStore, script: String) {
    let s = store.clone();
    group
        .run_mutation("corrupt temporary graph", move |c| async move {
            joined(c, move |c| {
                let guard = s.inner.lock().unwrap();
                let tx = Transaction::new(&guard.as_ref().unwrap().db, true);
                tx.run(&script, BTreeMap::new(), &c)?;
                tx.commit(&c)
            })
            .await
            .map_err(Into::into)
        })
        .await
        .unwrap();
}

fn snapshot(store: &CozoGraphStore) -> BTreeMap<String, Value> {
    let guard = store.inner.lock().unwrap();
    let tx = Transaction::new(&guard.as_ref().unwrap().db, false);
    let rows = SCHEMAS
        .iter()
        .map(|(name, _, columns)| {
            let fields = columns
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");
            let rows = tx
                .run(
                    &format!("?[{fields}] := *{name}{{{fields}}}"),
                    BTreeMap::new(),
                    &Cancellation::default(),
                )
                .unwrap();
            (name.to_string(), rows.into_json()["rows"].clone())
        })
        .collect();
    tx.abort().unwrap();
    rows
}

fn assert_reverse(store: &CozoGraphStore, expected_edges: usize) {
    let guard = store.inner.lock().unwrap();
    let tx = Transaction::new(&guard.as_ref().unwrap().db, false);
    let c = Cancellation::default();
    super::super::incidence::check_all(&tx, &c).unwrap();
    let mut count = 0;
    for (forward, reverse, node) in [
        ("authored_by", "author_paper", "author_id"),
        ("has_topic", "topic_paper", "topic_id"),
    ] {
        let read = |relation| {
            strings(
                tx.run(
                    &format!("?[paper_id, {node}] := *{relation}{{paper_id, {node}}}"),
                    BTreeMap::new(),
                    &c,
                )
                .unwrap(),
            )
            .unwrap()
        };
        let rows = read(forward);
        assert_eq!(rows, read(reverse));
        count += rows.len();
    }
    assert_eq!(count, expected_edges);
    tx.abort().unwrap();
}

#[tokio::test]
async fn tracking_fresh_publication_and_reopen_are_read_only() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    let initial = store
        .observe_publication(Cancellation::default())
        .await
        .unwrap();
    assert_eq!(initial.generation(), 0);
    assert_eq!(initial.kind(), GraphWriteKind::Initialize);
    assert_eq!(initial.journal_authority(), GraphJournalAuthority::Absent);
    assert_eq!(
        uuid::Uuid::parse_str(initial.write_id())
            .unwrap()
            .get_version(),
        Some(uuid::Version::Random)
    );
    let observation = store
        .observe_paper_publication("p".into(), Cancellation::default())
        .await
        .unwrap();
    assert_eq!(observation.publication(), &initial);
    assert!(observation.artifact().paper().is_none());
    assert_reverse(&store, 0);
    store.close().await.unwrap();
    let bytes = fs::read(root.path().join("graph.sqlite3")).unwrap();
    let store = open(root.path()).await;
    assert_eq!(
        store
            .observe_publication(Cancellation::default())
            .await
            .unwrap(),
        initial
    );
    store.close().await.unwrap();
    assert_eq!(fs::read(root.path().join("graph.sqlite3")).unwrap(), bytes);
}

#[tokio::test]
async fn tracking_separate_batch_identical_noop_restore_and_reset_commits() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let store = open(root.path()).await;
        let mut ids = BTreeSet::new();
        ids.insert(
            store
                .observe_publication(group.preparation_stop())
                .await
                .unwrap()
                .write_id()
                .to_owned(),
        );
        let batches = [
            vec![replace("p", "Alice")],
            vec![replace("p", "Alice")],
            vec![GraphOperation::Remove("missing".into())],
            vec![
                replace("q", "ALICE"),
                replace("r", "Bob"),
                GraphOperation::Remove("r".into()),
            ],
            vec![
                GraphOperation::Reset,
                replace("p", "Alice"),
                replace("p", "Alice"),
            ],
        ];
        for (index, ops) in batches.into_iter().enumerate() {
            apply(&group, &store, ops).await.unwrap();
            let meta = store
                .observe_publication(group.preparation_stop())
                .await
                .unwrap();
            assert_eq!(meta.generation(), index as i64 + 1);
            assert_eq!(meta.kind(), GraphWriteKind::Apply);
            assert_eq!(
                meta.journal_authority(),
                GraphJournalAuthority::ExternalUntrusted
            );
            assert!(ids.insert(meta.write_id().to_owned()));
            assert_reverse(&store, if index == 3 { 4 } else { 2 });
        }
        let artifact = store
            .observe_paper("p".into(), group.preparation_stop())
            .await
            .unwrap();
        let s = store.clone();
        group
            .run_mutation("identical guarded restore", move |c| async move {
                s.restore_paper(artifact, c).await.map_err(Into::into)
            })
            .await
            .unwrap();
        let meta = store
            .observe_publication(group.preparation_stop())
            .await
            .unwrap();
        assert_eq!(meta.generation(), 6);
        assert_eq!(meta.kind(), GraphWriteKind::Restore);
        assert!(ids.insert(meta.write_id().to_owned()));
        for generation in [7, 8] {
            let s = store.clone();
            group
                .run_mutation("reset even when empty", move |c| async move {
                    s.reset(c).await.map_err(Into::into)
                })
                .await
                .unwrap();
            let meta = store
                .observe_publication(group.preparation_stop())
                .await
                .unwrap();
            assert_eq!(meta.generation(), generation);
            assert_eq!(meta.kind(), GraphWriteKind::Reset);
            assert_eq!(
                meta.journal_authority(),
                GraphJournalAuthority::ExternalUntrusted
            );
            assert!(ids.insert(meta.write_id().to_owned()));
            assert_reverse(&store, 0);
        }
        let s = store.clone();
        group
            .run_mutation("restore absence", move |c| async move {
                s.restore_paper(PaperRecoveryArtifact::absent("p".into()).unwrap(), c)
                    .await
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        let before = snapshot(&store);
        assert_eq!(
            store
                .observe_publication(group.preparation_stop())
                .await
                .unwrap()
                .generation(),
            9
        );
        assert_eq!(
            apply(&group, &store, vec![]).await,
            Err(GraphError::InvalidInput)
        );
        assert_eq!(
            apply(&group, &store, vec![GraphOperation::Remove("".into())]).await,
            Err(GraphError::InvalidInput)
        );
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert_eq!(
            store.observe_publication(cancelled).await,
            Err(GraphError::Cancelled)
        );
        assert_eq!(snapshot(&store), before);
        store.close().await.unwrap();
        let store = open(root.path()).await;
        assert_eq!(snapshot(&store), before);
    })
    .await;
}

#[tokio::test]
async fn tracking_v1_empty_and_nonempty_require_rebuild_without_conversion() {
    for nonempty in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.sqlite3");
        let db = DbInstance::new("sqlite", &path, "").unwrap();
        for (_, script, _) in &SCHEMAS[..6] {
            db.run_default(script).unwrap();
        }
        db.run_default(
            "?[key, version] <- [['electron-paper-graph', 1]] :put graph_meta {key => version}",
        )
        .unwrap();
        if nonempty {
            db.run_default(
                "?[paper_id, title] <- [['old', 'Keep me']] :put paper {paper_id => title}",
            )
            .unwrap();
        }
        drop(db);
        let bytes = fs::read(&path).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                CozoGraphStore::open(root.path().into(), path.clone(), Cancellation::default())
                    .await,
                Err(GraphError::RebuildRequired)
            ));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
    assert_eq!(
        ErrorCode::from(GraphError::RebuildRequired),
        ErrorCode::Internal
    );
    assert_eq!(
        ErrorCode::from(GraphError::GenerationOverflow),
        ErrorCode::Internal
    );
}

#[tokio::test]
async fn tracking_reopen_validates_marker_relation_keys_and_types() {
    for scripts in [
        vec!["::remove author_paper"],
        vec![
            "::remove topic_paper",
            ":create topic_paper {paper_id: String, topic_id: String}",
        ],
        vec![
            "::remove graph_publication",
            ":create graph_publication {key: String => version: Int, generation: String, write_id: String, transition: String, authority: String}",
        ],
        vec!["?[key, version] <- [['electron-paper-graph', 1]] :put graph_meta {key => version}"],
        vec!["?[key, version] <- [['electron-paper-graph', 99]] :put graph_meta {key => version}"],
        vec!["?[key, version] <- [['extra', 2]] :put graph_meta {key => version}"],
        vec!["?[key] := *graph_meta{key} :rm graph_meta {key}"],
        vec![":create unrelated {id: Int}"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.sqlite3");
        open(root.path()).await.close().await.unwrap();
        let db = DbInstance::new("sqlite", &path, "").unwrap();
        for script in scripts {
            db.run_default(script).unwrap();
        }
        drop(db);
        let bytes = fs::read(&path).unwrap();
        assert!(matches!(
            CozoGraphStore::open(root.path().into(), path.clone(), Cancellation::default()).await,
            Err(GraphError::InvalidSchema)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn tracking_bad_metadata_and_overflow_refuse_all_writes_without_mutation() {
    fixtures::with_group(|group| async move {
        let id = "00000000-0000-4000-8000-000000000001";
        let cases = [
            ("store", 2, 1, id, "apply", "external"),
            ("wrong", 1, 1, id, "apply", "external"),
            ("store", 1, -1, id, "apply", "external"),
            ("store", 1, 1, "not-a-uuid", "apply", "external"),
            ("store", 1, 1, id, "initialize", "absent"),
            ("store", 1, 0, id, "apply", "external"),
            ("store", 1, 1, id, "apply", "journal"),
            ("store", 1, 1, id, "unknown", "external"),
            ("store", 1, i64::MAX, id, "apply", "external"),
        ];
        let mut scripts = vec![
            "?[key] := *graph_publication{key} :rm graph_publication {key}".to_string(),
            format!("?[key, version, generation, write_id, transition, authority] <- [['extra', 1, 1, '{id}', 'apply', 'external']] :put graph_publication {{key => version, generation, write_id, transition, authority}}"),
        ];
        scripts.extend(cases.into_iter().map(|(key,v,g,id,t,a)| format!("?[key, version, generation, write_id, transition, authority] <- [['{key}', {v}, {g}, '{id}', '{t}', '{a}']] :put graph_publication {{key => version, generation, write_id, transition, authority}}")));
        let last = scripts.len()-1;
        for (index, script) in scripts.into_iter().enumerate() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("graph.sqlite3");
            let store = open(root.path()).await;
            apply(&group, &store, vec![replace("p", "Before")]).await.unwrap();
            raw_write(&group, &store, script).await;
            let before = snapshot(&store);
            let bytes = fs::read(&path).unwrap();
            let error = if index == last { GraphError::GenerationOverflow } else { GraphError::InvalidSchema };
            if index == last {
                assert_eq!(store.observe_publication(group.preparation_stop()).await.unwrap().generation(), i64::MAX);
            } else {
                assert_eq!(store.observe_publication(group.preparation_stop()).await, Err(error));
                assert_eq!(store.observe_paper_publication("p".into(), group.preparation_stop()).await, Err(error));
            }
            for ops in [vec![replace("p", "After")], vec![GraphOperation::Remove("p".into())], vec![GraphOperation::Reset]] {
                assert_eq!(apply(&group, &store, ops).await, Err(error));
            }
            let s = store.clone();
            assert_eq!(group.run_mutation("refuse restore", move |c| async move { Ok(s.restore_paper(PaperRecoveryArtifact::absent("p".into()).unwrap(), c).await) }).await.unwrap(), Err(error));
            assert_eq!(snapshot(&store), before);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            store.close().await.unwrap();
            let reopened = CozoGraphStore::open(root.path().into(), path.clone(), Cancellation::default()).await;
            if index == last { reopened.unwrap().close().await.unwrap(); }
            else { assert!(matches!(reopened, Err(GraphError::InvalidSchema))); }
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }).await;
}

#[tokio::test]
async fn tracking_reverse_relations_preserve_shared_labels_and_affected_only_restore() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let store = open(root.path()).await;
        apply(&group, &store, vec![replace("p", "Straße")]).await.unwrap();
        let old = store.observe_paper("p".into(), group.preparation_stop()).await.unwrap();
        apply(&group, &store, vec![replace("q", "STRASSE")]).await.unwrap();
        assert_reverse(&store, 4);
        assert_eq!(store.observe_paper("p".into(), group.preparation_stop()).await.unwrap().paper().unwrap().authors[0].name, "STRASSE");
        let before = snapshot(&store);
        let s = store.clone();
        let artifact = old.clone();
        assert_eq!(group.run_mutation("shared conflict", move |c| async move { Ok(s.restore_paper(artifact,c).await) }).await.unwrap(), Err(GraphError::SharedNodeConflict));
        assert_eq!(snapshot(&store), before);
        apply(&group, &store, vec![GraphOperation::Remove("q".into())]).await.unwrap();
        assert_reverse(&store, 2);
        assert_eq!(store.observe_paper("p".into(), group.preparation_stop()).await.unwrap().paper().unwrap().topics[0].name, "STRASSE");
        let orphan = stable_node_id(NodeKind::Author, "Orphan").unwrap();
        raw_write(&group, &store, format!("?[author_id, name, normalized_name] <- [['{orphan}', 'Orphan', 'orphan']] :put author {{author_id => name, normalized_name}}")).await;
        let s = store.clone();
        group.run_mutation("isolated restore", move |c| async move { s.restore_paper(old,c).await.map_err(Into::into) }).await.unwrap();
        assert_reverse(&store, 2);
        assert_eq!(store.status(group.preparation_stop()).await.unwrap().authors, 2);
        let s = store.clone();
        group.run_mutation("guarded removal", move |c| async move { s.restore_paper(PaperRecoveryArtifact::absent("p".into()).unwrap(), c).await.map_err(Into::into) }).await.unwrap();
        assert_reverse(&store, 0);
        assert_eq!(store.status(group.preparation_stop()).await.unwrap().authors, 1);
        apply(&group, &store, vec![replace("r", "Other"), replace("r", "New")]).await.unwrap();
        assert_reverse(&store, 2);
        assert_eq!(store.status(group.preparation_stop()).await.unwrap().authors, 1);
        apply(&group, &store, vec![GraphOperation::Remove("r".into())]).await.unwrap();
        assert_reverse(&store, 0);
        assert_eq!(store.status(group.preparation_stop()).await.unwrap().authors, 0);
    }).await;
}

#[tokio::test]
async fn tracking_reverse_corruption_is_not_hidden_or_repaired() {
    fixtures::with_group(|group| async move {
        for relation in ["authored_by", "author_paper", "has_topic", "topic_paper"] {
            for extra in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let store = open(root.path()).await;
                apply(&group, &store, vec![replace("p", "Shared")]).await.unwrap();
                let node = if relation.contains("topic") { "topic_id" } else { "author_id" };
                let script = if extra {
                    format!("?[paper_id, {node}] <- [['p', 'extra']] :put {relation} {{paper_id, {node}}}")
                } else {
                    format!("?[paper_id, {node}] := *{relation}{{paper_id, {node}}} :rm {relation} {{paper_id, {node}}}")
                };
                raw_write(&group, &store, script).await;
                let before = snapshot(&store);
                assert_eq!(store.observe_paper("p".into(), group.preparation_stop()).await, Err(GraphError::InvalidSchema));
                for ops in [vec![replace("p", "Other")], vec![GraphOperation::Remove("p".into())], vec![GraphOperation::Reset]] {
                    assert_eq!(apply(&group, &store, ops).await, Err(GraphError::InvalidSchema));
                }
                let s = store.clone();
                assert_eq!(group.run_mutation("target consistency", move |c| async move { Ok(s.restore_paper(PaperRecoveryArtifact::absent("p".into()).unwrap(),c).await) }).await.unwrap(), Err(GraphError::InvalidSchema));
                if !extra {
                    let s = store.clone();
                    let desired = PaperRecoveryArtifact::from_prepared(&PreparedPaper::new("q", "Other", &["Shared".into()], &["Shared".into()]).unwrap()).unwrap();
                    assert_eq!(group.run_mutation("affected node consistency", move |c| async move { Ok(s.restore_paper(desired,c).await) }).await.unwrap(), Err(GraphError::InvalidSchema));
                }
                assert_eq!(snapshot(&store), before);
            }
        }
    }).await;
}

#[tokio::test]
async fn tracking_staged_failure_rolls_back_graph_reverse_and_fence() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let store = open(root.path()).await;
        apply(&group, &store, vec![replace("p", "Before")])
            .await
            .unwrap();
        let before = snapshot(&store);
        let bytes = fs::read(root.path().join("graph.sqlite3")).unwrap();
        let s = store.clone();
        let result = group
            .run_mutation("fail after staging", move |c| async move {
                Ok(joined(c, move |c| {
                    s.apply_sync(
                        vec![GraphOperation::Reset, replace("p", "After")],
                        &c,
                        || Err(GraphError::Storage),
                    )
                })
                .await)
            })
            .await
            .unwrap();
        assert_eq!(result, Err(GraphError::Storage));
        assert_eq!(snapshot(&store), before);
        assert_reverse(&store, 2);
        assert_eq!(fs::read(root.path().join("graph.sqlite3")).unwrap(), bytes);
        store.close().await.unwrap();
        assert_eq!(snapshot(&open(root.path()).await), before);
    })
    .await;
}

#[tokio::test]
async fn tracking_paper_and_fence_observation_serializes_with_writer() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let store = open(root.path()).await;
        apply(&group, &store, vec![replace("p", "Before")])
            .await
            .unwrap();
        let before = store
            .observe_paper_publication("p".into(), group.preparation_stop())
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
        let read =
            tokio::spawn(async move { reader.observe_paper_publication("p".into(), c).await });
        started.await.unwrap();
        let s = store.clone();
        let g = group.clone();
        let (admit, admitted) = tokio::sync::oneshot::channel();
        let write = tokio::spawn(async move {
            g.run_mutation("concurrent publication", move |c| async move {
                admit.send(()).unwrap();
                s.apply(vec![replace("p", "After")], c)
                    .await
                    .map_err(Into::into)
            })
            .await
        });
        admitted.await.unwrap();
        assert!(store.inner.try_lock().is_err());
        assert!(!write.is_finished());
        release.send(()).unwrap();
        assert_eq!(read.await.unwrap().unwrap(), before);
        write.await.unwrap().unwrap();
        let after = store
            .observe_paper_publication("p".into(), group.preparation_stop())
            .await
            .unwrap();
        assert_eq!(before.publication().generation(), 1);
        assert_eq!(after.publication().generation(), 2);
        assert_eq!(after.artifact().paper().unwrap().title, "After");
        assert_ne!(
            before.publication().write_id(),
            after.publication().write_id()
        );
        assert_reverse(&store, 2);
    })
    .await;
}

struct ThreadExit(Arc<AtomicBool>);
impl Drop for ThreadExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
thread_local! { static EXIT: RefCell<Option<ThreadExit>> = const { RefCell::new(None) }; }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracking_pre_post_seal_cancel_and_drop_join_actual_worker() {
    fixtures::with_group(|group| async move {
        for mode in ["apply", "restore", "reset"] {
            for sealed in [false, true] {
                for abort in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let path = root.path().join("graph.sqlite3");
                    let store = open(root.path()).await;
                    apply(&group, &store, vec![replace("p", "Before")])
                        .await
                        .unwrap();
                    let before = snapshot(&store);
                    let bytes = fs::read(&path).unwrap();
                    let exited = Arc::new(AtomicBool::new(false));
                    let flag = exited.clone();
                    let (start, started) = tokio::sync::oneshot::channel();
                    let start = Mutex::new(Some(start));
                    let (release, released) = mpsc::channel();
                    let released = Mutex::new(released);
                    let mut writer = store.clone();
                    writer.write_hook = Some(Arc::new(move |after_seal, c| {
                        if after_seal == sealed {
                            EXIT.with(|exit| *exit.borrow_mut() = Some(ThreadExit(flag.clone())));
                            assert!(
                                start
                                    .lock()
                                    .unwrap()
                                    .take()
                                    .unwrap()
                                    .send(c.clone())
                                    .is_ok()
                            );
                            released.lock().unwrap().recv().unwrap();
                        }
                    }));
                    let (abort_send, abort_receive) = tokio::sync::oneshot::channel();
                    let g = group.clone();
                    let write = tokio::spawn(async move {
                        g.run_mutation("cancel or drop tracked native write", move |c| async move {
                            let child = tokio::spawn(async move {
                                match mode {
                                    "apply" => {
                                        writer
                                            .apply(
                                                vec![GraphOperation::Reset, replace("p", "After")],
                                                c,
                                            )
                                            .await
                                    }
                                    "restore" => {
                                        writer
                                            .restore_paper(
                                                PaperRecoveryArtifact::from_prepared(
                                                    &PreparedPaper::new(
                                                        "p",
                                                        "After",
                                                        &["After".into()],
                                                        &["After".into()],
                                                    )
                                                    .unwrap(),
                                                )
                                                .unwrap(),
                                                c,
                                            )
                                            .await
                                    }
                                    _ => writer.reset(c).await,
                                }
                            });
                            abort_send.send(child.abort_handle()).unwrap();
                            Ok(child.await)
                        })
                        .await
                    });
                    let token = started.await.unwrap();
                    let abort_handle = abort_receive.await.unwrap();
                    assert!(!exited.load(Ordering::SeqCst));
                    assert!(store.inner.try_lock().is_err());
                    if abort {
                        abort_handle.abort();
                        if !sealed {
                            while token.check().is_ok() {
                                tokio::task::yield_now().await;
                            }
                        }
                    } else {
                        assert_eq!(token.cancel(), !sealed);
                    }
                    // While the native barrier is held, neither cancellation nor
                    // Drop can report completed work or release the graph mutex.
                    assert!(!write.is_finished());
                    assert!(!exited.load(Ordering::SeqCst));
                    release.send(()).unwrap();
                    let result = write.await.unwrap().unwrap();
                    if abort {
                        assert!(result.unwrap_err().is_cancelled());
                    } else {
                        assert_eq!(
                            result.unwrap(),
                            if sealed {
                                Ok(())
                            } else {
                                Err(GraphError::Cancelled)
                            }
                        );
                    }
                    assert!(
                        exited.load(Ordering::SeqCst),
                        "completion must join native thread teardown"
                    );
                    sqlite::open(&path)
                        .unwrap()
                        .execute("BEGIN EXCLUSIVE; ROLLBACK")
                        .unwrap();
                    if sealed {
                        let meta = store
                            .observe_publication(group.preparation_stop())
                            .await
                            .unwrap();
                        assert_eq!(meta.generation(), 2);
                        assert_eq!(
                            meta.journal_authority(),
                            GraphJournalAuthority::ExternalUntrusted
                        );
                        assert_reverse(&store, if mode == "reset" { 0 } else { 2 });
                        assert_ne!(snapshot(&store), before);
                    } else {
                        assert_eq!(snapshot(&store), before);
                        assert_reverse(&store, 2);
                        assert_eq!(fs::read(&path).unwrap(), bytes);
                    }
                    let after = snapshot(&store);
                    store.close().await.unwrap();
                    assert_eq!(snapshot(&open(root.path()).await), after);
                }
            }
        }
    })
    .await;
}

#[tokio::test]
async fn tracking_cancel_before_start_and_failed_native_commit_keep_fence() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.sqlite3");
        let store = open(root.path()).await;
        apply(&group, &store, vec![replace("p", "Before")])
            .await
            .unwrap();
        let before = snapshot(&store);
        let s = store.clone();
        let result = group
            .run_mutation("cancel before store entry", move |c| async move {
                assert!(c.cancel());
                Ok(s.apply(vec![replace("p", "After")], c).await)
            })
            .await
            .unwrap();
        assert_eq!(result, Err(GraphError::Cancelled));
        assert_eq!(snapshot(&store), before);
        let reader = sqlite::open(&path).unwrap();
        reader.execute("BEGIN; SELECT count(*) FROM cozo").unwrap();
        let s = store.clone();
        let (result, late_cancel) = group
            .run_mutation("real failed sqlite commit", move |c| async move {
                let result = s.apply(vec![replace("p", "After")], c.clone()).await;
                Ok((result, c.cancel()))
            })
            .await
            .unwrap();
        assert_eq!(result, Err(GraphError::Storage));
        assert!(!late_cancel);
        reader.execute("ROLLBACK").unwrap();
        assert_eq!(snapshot(&store), before);
        assert_reverse(&store, 2);
        store.close().await.unwrap();
        assert_eq!(snapshot(&open(root.path()).await), before);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracking_concurrent_requests_do_not_lose_generation_increments() {
    let root = tempfile::tempdir().unwrap();
    let store = open(root.path()).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut writers = Vec::new();
    for n in 0..8 {
        let s = store.clone();
        let barrier = barrier.clone();
        writers.push(tokio::spawn(fixtures::with_group(
            move |group| async move {
                barrier.wait().await;
                apply(&group, &s, vec![replace(&format!("p{n}"), "Shared")])
                    .await
                    .unwrap();
            },
        )));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    assert_eq!(
        store
            .observe_publication(Cancellation::default())
            .await
            .unwrap()
            .generation(),
        8
    );
    assert_eq!(
        store.status(Cancellation::default()).await.unwrap().papers,
        8
    );
    assert_reverse(&store, 16);
    let before = snapshot(&store);
    store.close().await.unwrap();
    assert_eq!(snapshot(&open(root.path()).await), before);
}
