use super::*;
#[path = "exchange_tests.rs"]
mod exchange_tests;
use crate::retrieval::recovery_fixtures as fixtures;
use crate::{graph as g, indexing::Readiness, retrieval as r};

use crate::{
    protocol::ErrorCode,
    registry::{BackendFuture, CommandHandler, Registry},
    runtime::{CancellationMode, RequestContext, serve},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Probe<F>(Mutex<Option<F>>, Arc<AtomicBool>);
impl<F, Fut> CommandHandler for Probe<F>
where
    F: FnOnce(OperationGroup) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Value, ErrorCode>> + Send + 'static,
{
    fn cancellation_mode(&self) -> CancellationMode {
        CancellationMode::Cooperative
    }
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        let f = self.0.lock().unwrap().take().unwrap();
        let done = self.1.clone();
        Box::pin(async move {
            let result = f(context.operation_group()?).await;
            done.store(true, Ordering::SeqCst);
            result
        })
    }
}
async fn probe<F, Fut>(expected: &str, f: F)
where
    F: FnOnce(OperationGroup) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Value, ErrorCode>> + Send + 'static,
{
    let done = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::foundation();
    registry
        .install(
            "index_papers_stream",
            "indexing",
            Arc::new(Probe(Mutex::new(Some(f)), done.clone())),
        )
        .unwrap();
    let (client, server) = tokio::io::duplex(8192);
    let (r, w) = tokio::io::split(server);
    let task = tokio::spawn(serve(r, w, registry));
    let (r, mut w) = tokio::io::split(client);
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"v\":1,\"type\":\"request\",\"id\":\"probe\",\"command\":\"index_papers_stream\",\"params\":{}}\n").await.unwrap();
    let line = tokio::time::timeout(std::time::Duration::from_secs(20), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], expected, "{v}");
    assert!(
        done.load(Ordering::SeqCst),
        "probe panicked before assertions completed"
    );
    w.shutdown().await.unwrap();
    assert!(lines.next_line().await.unwrap().is_none());
    task.await.unwrap().unwrap();
}

fn identity() -> EmbeddingIdentity {
    EmbeddingIdentity {
        model: "fake/model/mode".into(),
        dimension: 2,
    }
}
fn paths(root: &std::path::Path) -> StoragePaths {
    let data = root.join("data");
    let papers = root.join("papers");
    let resources = root.join("resources");
    for p in [&data, &papers, &resources] {
        std::fs::create_dir_all(p).unwrap();
    }
    for id in ["p", "q"] {
        std::fs::write(
            papers.join(format!("{id}.pdf")),
            b"%PDF-1.4\nsynthetic snapshot",
        )
        .unwrap();
    }
    StoragePaths {
        data_root: data.clone(),
        history_database: data.join("history.sqlite3"),
        vector_database: data.join("vectors.sqlite3"),
        graph_database: data.join("graph.sqlite3"),
        index_state_database: data.join("index-state.sqlite3"),
        bm25_index: data.join("bm25.json"),
        evaluation_results: data.join("evaluations"),
        resources,
        papers,
    }
}
fn prepared(id: &str, name: &str, text: &str, expected: Option<String>) -> PreparedPublication {
    let chunk = r::EmbeddedChunk {
        id: format!("{id}:0"),
        paper_id: id.into(),
        chunk_index: 0,
        filename: format!("{id}.pdf"),
        title: "Title".into(),
        page: 1,
        text: text.into(),
        vector: vec![0.25, 0.5],
    };
    let retrieval = RetrievalArtifact::from_prepared(
        &r::PreparedPaper::new(id.into(), identity(), vec![chunk]).unwrap(),
    )
    .unwrap();
    let graph = GraphArtifact::from_prepared(
        &g::PreparedPaper::new(id, "Title", &[name.into()], &[name.into()]).unwrap(),
    )
    .unwrap();
    PreparedPublication::new(
        journal::digest(b"%PDF-1.4\nsynthetic snapshot"),
        expected,
        retrieval,
        graph,
    )
    .unwrap()
}
async fn operation(index: &CoordinatedIndex, p: PreparedPublication) -> Operation {
    let (r, g) = p.version.validate().unwrap();
    let prior = index.journal.active(r.paper_id()).unwrap();
    index.build_operation(p.version, prior, g).await.unwrap()
}
async fn stage(index: &CoordinatedIndex, op: &Operation, steps: usize) {
    let Kind::Paper(p) = &op.kind else { panic!() };
    index.journal.prepare(op, &Cancellation::default()).unwrap();
    if steps >= 1 {
        index
            .restore(
                p.desired.validate().unwrap().0,
                Lane::Startup,
                op.base.revision,
            )
            .await
            .unwrap();
    }
    if steps >= 2 {
        index
            .journal
            .phase(&op.id, Phase::RetrievalApplied, &Cancellation::default())
            .unwrap();
    }
    if steps >= 3 {
        index
            .graph
            .transition(p.transition.clone(), false, Cancellation::default())
            .await
            .unwrap();
    }
    if steps >= 4 {
        index
            .journal
            .phase(&op.id, Phase::GraphApplied, &Cancellation::default())
            .unwrap();
    }
    if steps >= 5 {
        index
            .finish_exact(op, true, p.transition.forward.clone(), Lane::Startup)
            .await
            .unwrap();
    }
}
async fn reopen(index: CoordinatedIndex, paths: &StoragePaths) -> CoordinatedIndex {
    index.graph.close().await.unwrap();
    drop(index);
    CoordinatedIndex::open(paths, identity()).await.unwrap()
}

#[tokio::test]
async fn journal_starting_ready_and_shared_labels_survive_reopen() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        assert_eq!(index.gate.readiness(), Readiness::Starting);
        assert!(index.gate.read().await.is_err());
        index.recover().await.unwrap();
        let p = index
            .publish(prepared("p", "Alice", "first", None), &group)
            .await
            .unwrap();
        index
            .publish(prepared("q", "ALICE", "second", None), &group)
            .await
            .unwrap();
        assert_eq!(index.published_paper("p".into()).await.unwrap().unwrap(), p);
        let observed = index
            .graph
            .observe_paper("p".into(), Cancellation::default())
            .await
            .unwrap();
        assert_eq!(observed.paper().unwrap().authors[0].name, "ALICE");
        let index = reopen(index, &paths).await;
        index.recover().await.unwrap();
        index.recover().await.unwrap();
        assert_eq!(index.fence().await.unwrap().generation, 2);
        assert_eq!(index.published_paper("p".into()).await.unwrap().unwrap(), p);
    })
    .await;
}

#[tokio::test]
async fn journal_every_forward_crash_boundary_recovers_idempotently() {
    for steps in 0..=5 {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let op = operation(&index, prepared("p", "Alice", "first", None)).await;
        stage(&index, &op, steps).await;
        let index = reopen(index, &paths).await;
        index.recover().await.unwrap();
        let fence = index.fence().await.unwrap();
        let revision = index.status().await.unwrap().revision;
        index.recover().await.unwrap();
        assert_eq!(index.fence().await.unwrap(), fence);
        assert_eq!(index.status().await.unwrap().revision, revision);
        assert_eq!(fence.generation, 1);
        assert_eq!(
            index.journal.operation().unwrap().unwrap().phase,
            Phase::Complete
        );
    }
}

#[tokio::test]
async fn journal_source_change_compensates_current_global_labels_not_old_paper_labels() {
    fixtures::with_group(|group| async move {
        for steps in 0..=5 {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
            index.recover().await.unwrap();
            let first = index
                .publish(prepared("p", "Alice", "first", None), &group)
                .await
                .unwrap();
            index
                .publish(prepared("q", "ALICE", "second", None), &group)
                .await
                .unwrap();
            let op = operation(
                &index,
                prepared("p", "Alice", "changed", Some(first.version_id.clone())),
            )
            .await;
            stage(&index, &op, steps).await;
            std::fs::write(paths.papers.join("p.pdf"), b"source changed after crash").unwrap();
            let index = reopen(index, &paths).await;
            index.recover().await.unwrap();
            index.recover().await.unwrap();
            let q = index
                .graph
                .observe_paper("q".into(), Cancellation::default())
                .await
                .unwrap();
            if steps < 5 {
                assert_eq!(q.paper().unwrap().authors[0].name, "ALICE");
                assert_eq!(
                    index.published_paper("p".into()).await.unwrap().unwrap(),
                    first
                );
                assert_eq!(
                    index.fence().await.unwrap().generation,
                    if steps >= 3 { 4 } else { 2 }
                );
            } else {
                assert_eq!(q.paper().unwrap().authors[0].name, "Alice");
                assert_eq!(index.fence().await.unwrap().generation, 3);
            }
        }
    })
    .await;
}

#[tokio::test]
async fn journal_reverse_and_abort_response_loss_never_rewinds_or_replays() {
    for reverse_stage in 0..=3 {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let op = operation(&index, prepared("p", "Alice", "first", None)).await;
        stage(&index, &op, 4).await;
        let Kind::Paper(p) = &op.kind else { panic!() };
        index
            .journal
            .phase(&op.id, Phase::Compensating, &Cancellation::default())
            .unwrap();
        if reverse_stage >= 1 {
            index
                .graph
                .transition(p.transition.clone(), true, Cancellation::default())
                .await
                .unwrap();
        }
        if reverse_stage >= 2 {
            index
                .restore(p.before_retrieval().unwrap(), Lane::Startup, 1)
                .await
                .unwrap();
        }
        if reverse_stage >= 3 {
            index
                .finish_exact(&op, false, p.transition.reverse.clone(), Lane::Startup)
                .await
                .unwrap();
        }
        let index = reopen(index, &paths).await;
        index.recover().await.unwrap();
        index.recover().await.unwrap();
        assert_eq!(index.fence().await.unwrap().generation, 2);
        assert!(index.published_paper("p".into()).await.unwrap().is_none());
        assert!(
            index
                .graph
                .transition(p.transition.clone(), true, Cancellation::default())
                .await
                .is_err()
        );
        assert_eq!(index.fence().await.unwrap().generation, 2);
    }
}

#[tokio::test]
async fn journal_reset_crash_boundaries_and_containment() {
    fixtures::with_group(|group| async move {
        for steps in 0..=6 {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            std::fs::write(
                &paths.history_database,
                b"canonical history and imported status sentinel",
            )
            .unwrap();
            let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
            index.recover().await.unwrap();
            index
                .publish(prepared("p", "Alice", "first", None), &group)
                .await
                .unwrap();
            let base = index.journal.meta().unwrap().unwrap();
            let next = base
                .fence
                .next(
                    "coordinated_reset",
                    journal::digest(b"empty-derived-index-v1"),
                )
                .unwrap();
            let op = Operation {
                id: uuid::Uuid::new_v4().to_string(),
                base: base.clone(),
                phase: Phase::ResetPrepared,
                kind: Kind::Reset { next: next.clone() },
            };
            index
                .journal
                .prepare(&op, &Cancellation::default())
                .unwrap();
            if steps >= 1 {
                index
                    .journal
                    .phase(&op.id, Phase::ResetFinalizing, &Cancellation::default())
                    .unwrap();
            }
            if steps >= 2 {
                index
                    .retrieval
                    .apply(
                        &[
                            PreparedOperation::Reset(PreparedReset::new(identity()).unwrap()),
                            PreparedOperation::RebuildLexical,
                        ],
                        Some(base.revision),
                        &Cancellation::default(),
                    )
                    .unwrap();
            }
            if steps >= 3 {
                index
                    .journal
                    .phase(&op.id, Phase::ResetRetrieval, &Cancellation::default())
                    .unwrap();
            }
            if steps >= 4 {
                index
                    .graph
                    .coordinated_reset(base.fence, next, Cancellation::default())
                    .await
                    .unwrap();
            }
            if steps >= 5 {
                index
                    .journal
                    .phase(&op.id, Phase::ResetGraph, &Cancellation::default())
                    .unwrap();
            }
            if steps >= 6 {
                index.complete_reset(&op, Lane::Startup).await.unwrap();
            }
            let index = reopen(index, &paths).await;
            index.recover().await.unwrap();
            let fence = index.fence().await.unwrap();
            index.recover().await.unwrap();
            assert_eq!(index.fence().await.unwrap(), fence);
            assert_eq!(
                index.status().await.unwrap().chunk_count,
                usize::from(steps == 0)
            );
            assert_eq!(
                std::fs::read(&paths.history_database).unwrap(),
                b"canonical history and imported status sentinel"
            );
            assert_eq!(
                std::fs::read(paths.papers.join("p.pdf")).unwrap(),
                b"%PDF-1.4\nsynthetic snapshot"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn journal_unknown_external_and_corrupt_pending_state_poison_without_repair() {
    for corrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let op = operation(&index, prepared("p", "Alice", "first", None)).await;
        stage(&index, &op, 1).await;
        if corrupt {
            sqlite::open(&paths.index_state_database)
                .unwrap()
                .execute("UPDATE operations SET payload='{}'")
                .unwrap();
        } else {
            index
                .graph
                .apply(
                    vec![g::GraphOperation::Remove("other".into())],
                    Cancellation::default(),
                )
                .await
                .unwrap();
        }
        let before = index.fence().await.unwrap();
        assert!(index.recover().await.is_err());
        assert_eq!(index.gate.readiness(), Readiness::Poisoned);
        assert!(index.gate.read().await.is_err());
        assert_eq!(index.fence().await.unwrap(), before);
        assert!(index.recover().await.is_err());
    }
}

#[tokio::test]
async fn journal_unjournaled_nonempty_store_requires_explicit_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
    index
        .graph
        .apply(
            vec![g::GraphOperation::Replace(
                g::PreparedPaper::new("p", "Title", &["Alice".into()], &["RAG".into()]).unwrap(),
            )],
            Cancellation::default(),
        )
        .await
        .unwrap();
    let before = index.fence().await.unwrap();
    assert_eq!(index.recover().await, Err(IndexError::RebuildRequired));
    assert_eq!(index.fence().await.unwrap(), before);
    assert!(index.journal.meta().unwrap().is_none());
}

#[tokio::test]
async fn journal_gate_serializes_read_write_and_poison_is_sticky_on_drop() {
    let gate = DerivedStoreGate::new();
    let mut initial = gate.write(true).await.unwrap();
    initial.ready();
    drop(initial);
    let read = gate.read().await.unwrap();
    let g = gate.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut guard = g.write(false).await.unwrap();
        guard.arm();
        tx.send(()).unwrap();
    });
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    drop(read);
    rx.await.unwrap();
    task.await.unwrap();
    assert_eq!(gate.readiness(), Readiness::Poisoned);
    assert!(gate.read().await.is_err());
    let mut recovery = gate.write(true).await.unwrap();
    assert_eq!(gate.readiness(), Readiness::Recovering);
    recovery.ready();
    drop(recovery);
    assert!(gate.read().await.is_ok());
}

#[tokio::test]
async fn journal_cancel_at_every_journal_seal_retains_only_completed_publication() {
    for boundary in 1..=4 {
        for point in [
            journal::WritePoint::BeforeSeal,
            journal::WritePoint::AfterSeal,
            journal::WritePoint::AfterCommit,
        ] {
            probe("cancelled", move |group| async move {
                let root = tempfile::tempdir().unwrap();
                let paths = paths(root.path());
                let mut index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
                index.recover().await.unwrap();
                let count = Arc::new(AtomicUsize::new(0));
                let token = group.preparation_stop();
                index.journal.hook = Some(Arc::new(move |at, _| {
                    if at == point && count.fetch_add(1, Ordering::SeqCst) + 1 == boundary {
                        assert!(token.cancel());
                    }
                    Ok(())
                }));
                let result = index
                    .publish(prepared("p", "Alice", "first", None), &group)
                    .await;
                let published = boundary == 4 && point != journal::WritePoint::BeforeSeal;
                if published {
                    assert!(result.is_ok());
                } else {
                    assert_eq!(result, Err(IndexError::Cancelled));
                }
                assert_eq!(index.gate.readiness(), Readiness::Ready);
                assert_eq!(
                    index.published_paper("p".into()).await.unwrap().is_some(),
                    published
                );
                index.journal.hook = None;
                index.recover().await.unwrap();
                Err(ErrorCode::Cancelled)
            })
            .await;
        }
    }
}

#[tokio::test]
async fn journal_lost_commit_responses_reconcile_and_cleanup_failure_poisons() {
    for boundary in 1..=4 {
        probe("error", move |group| async move {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            let mut index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
            index.recover().await.unwrap();
            let count = AtomicUsize::new(0);
            index.journal.hook = Some(Arc::new(move |at, _| {
                if at == journal::WritePoint::AfterCommit
                    && count.fetch_add(1, Ordering::SeqCst) + 1 == boundary
                {
                    return Err(IndexError::Storage);
                }
                Ok(())
            }));
            assert!(
                index
                    .publish(prepared("p", "Alice", "first", None), &group)
                    .await
                    .is_err()
            );
            assert_eq!(index.gate.readiness(), Readiness::Ready);
            assert_eq!(
                index.published_paper("p".into()).await.unwrap().is_some(),
                boundary == 4
            );
            Err(ErrorCode::Internal)
        })
        .await;
    }
    probe("error", |group| async move {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let mut index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let count = AtomicUsize::new(0);
        index.journal.hook = Some(Arc::new(move |at, _| {
            if at == journal::WritePoint::AfterCommit && count.fetch_add(1, Ordering::SeqCst) >= 1 {
                return Err(IndexError::Storage);
            }
            Ok(())
        }));
        assert!(
            index
                .publish(prepared("p", "Alice", "first", None), &group)
                .await
                .is_err()
        );
        assert_eq!(index.gate.readiness(), Readiness::Poisoned);
        assert!(index.gate.read().await.is_err());
        index.journal.hook = None;
        index.recover().await.unwrap();
        assert_eq!(index.gate.readiness(), Readiness::Ready);
        Err(ErrorCode::Internal)
    })
    .await;
}

#[tokio::test]
async fn journal_linked_reset_refuses_late_cancel_and_clears_only_derived_state() {
    probe("result", |group| async move {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let mut index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        index
            .publish(prepared("p", "Alice", "first", None), &group)
            .await
            .unwrap();
        let count = AtomicUsize::new(0);
        let stop = group.preparation_stop();
        index.journal.hook = Some(Arc::new(move |at, _| {
            if at == journal::WritePoint::BeforeSeal && count.fetch_add(1, Ordering::SeqCst) >= 1 {
                assert!(!stop.cancel());
            }
            Ok(())
        }));
        index.reset(&group).await.unwrap();
        assert_eq!(index.status().await.unwrap().chunk_count, 0);
        assert_eq!(index.gate.readiness(), Readiness::Ready);
        Ok(json!({"done":true}))
    })
    .await;
    probe("cancelled", |group| async move {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let mut index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let once = AtomicBool::new(false);
        let stop = group.preparation_stop();
        index.journal.hook = Some(Arc::new(move |at, _| {
            if at == journal::WritePoint::AfterCommit && !once.swap(true, Ordering::SeqCst) {
                assert!(stop.cancel());
            }
            Ok(())
        }));
        assert_eq!(index.reset(&group).await, Err(IndexError::Cancelled));
        assert_eq!(index.gate.readiness(), Readiness::Ready);
        assert_eq!(index.fence().await.unwrap().generation, 0);
        Err(ErrorCode::Cancelled)
    })
    .await;
}

#[tokio::test]
async fn journal_identical_empty_delete_orphan_and_stale_preparation() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let p = index
            .publish(prepared("p", "Straße", "first", None), &group)
            .await
            .unwrap();
        assert_eq!(
            index
                .publish(prepared("p", "Wrong", "stale", None), &group)
                .await,
            Err(IndexError::Conflict)
        );
        let p = index
            .publish(prepared("p", "Straße", "first", Some(p.version_id)), &group)
            .await
            .unwrap();
        assert_eq!(index.status().await.unwrap().revision, 1);
        assert_eq!(index.fence().await.unwrap().generation, 2);
        let q = index
            .publish(prepared("q", "STRASSE", "second", None), &group)
            .await
            .unwrap();
        let empty = |id: &str, v: String| {
            PreparedPublication::new(
                journal::digest(b"%PDF-1.4\nsynthetic snapshot"),
                Some(v),
                RetrievalArtifact::no_rows(id.into(), identity()).unwrap(),
                GraphArtifact::absent(id.into()).unwrap(),
            )
            .unwrap()
        };
        index
            .publish(empty("q", q.version_id), &group)
            .await
            .unwrap();
        let graph = index
            .graph
            .observe_paper("p".into(), Cancellation::default())
            .await
            .unwrap();
        assert_eq!(graph.paper().unwrap().authors[0].name, "STRASSE");
        let empty_p = index
            .publish(empty("p", p.version_id), &group)
            .await
            .unwrap();
        assert!(empty_p.empty);
        assert_eq!(
            index
                .graph
                .status(Cancellation::default())
                .await
                .unwrap()
                .authors,
            0
        );
        index.recover().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn journal_concurrent_publication_admits_only_one_captured_base() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
    index.recover().await.unwrap();
    let a = index.clone();
    let b = index.clone();
    let wins = Arc::new(AtomicUsize::new(0));
    let w = wins.clone();
    let x = wins.clone();
    let left = fixtures::with_group(move |g| async move {
        if a.publish(prepared("p", "Alice", "a", None), &g)
            .await
            .is_ok()
        {
            w.fetch_add(1, Ordering::SeqCst);
        }
    });
    let right = fixtures::with_group(move |g| async move {
        if b.publish(prepared("p", "ALICE", "b", None), &g)
            .await
            .is_ok()
        {
            x.fetch_add(1, Ordering::SeqCst);
        }
    });
    tokio::join!(left, right);
    assert_eq!(wins.load(Ordering::SeqCst), 1);
    assert_eq!(index.fence().await.unwrap().generation, 1);
    index.recover().await.unwrap();
}

#[tokio::test]
async fn journal_native_worker_drop_joins_actual_thread_and_releases_sqlite_lock() {
    thread_local! {static EXIT:std::cell::RefCell<Option<Exit>>=const {std::cell::RefCell::new(None)};}
    struct Exit(Arc<AtomicBool>);
    impl Drop for Exit {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("worker.sqlite");
    sqlite::open(&path)
        .unwrap()
        .execute("CREATE TABLE probe(id INTEGER)")
        .unwrap();
    let worker_path = path.clone();
    let exited = Arc::new(AtomicBool::new(false));
    let flag = exited.clone();
    let (started, rx) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let (dropping, drop_started) = tokio::sync::oneshot::channel();
    let (done, finished) = tokio::sync::oneshot::channel();
    let runtime = tokio::runtime::Handle::current();
    let thread = std::thread::spawn(move || {
        runtime.block_on(async move {
            let mut future = Box::pin(joined(Cancellation::default(), move |_| {
                EXIT.with(|e| *e.borrow_mut() = Some(Exit(flag)));
                let db = sqlite::open(worker_path)?;
                db.execute("BEGIN EXCLUSIVE; INSERT INTO probe VALUES(1)")?;
                started.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            }));
            tokio::select! { _=&mut future=>panic!("worker returned before release"), _=rx=>() }
            dropping.send(()).unwrap();
            drop(future);
            done.send(()).unwrap();
        })
    });
    // Release is ordered after the native statement started; completion must
    // include connection rollback and the thread-local destructor, not just send.
    drop_started.await.unwrap();
    assert!(!exited.load(Ordering::SeqCst));
    release.send(()).unwrap();
    finished.await.unwrap();
    thread.join().unwrap();
    assert!(exited.load(Ordering::SeqCst));
    let db = sqlite::open(path).unwrap();
    db.execute("BEGIN EXCLUSIVE").unwrap();
    let mut s = db.prepare("SELECT count(*) FROM probe").unwrap();
    s.next().unwrap();
    assert_eq!(s.read::<i64, _>(0).unwrap(), 0);
}

#[tokio::test]
async fn journal_new_source_mismatch_preserves_publication_and_readiness() {
    fixtures::with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
        index.recover().await.unwrap();
        let p = prepared("p", "Alice", "first", None);
        let mut missing = p.clone();
        missing.version.source = journal::digest(b"not current source");
        assert_eq!(
            index.publish(missing, &group).await,
            Err(IndexError::Conflict)
        );
        assert!(index.journal.operation().unwrap().is_none());
        assert_eq!(index.gate.readiness(), Readiness::Ready);
        index.publish(p, &group).await.unwrap();
        let index = reopen(index, &paths).await;
        index.recover().await.unwrap();
        assert!(index.published_paper("p".into()).await.unwrap().is_some());
    })
    .await;
}

#[tokio::test]
async fn journal_reset_failure_before_seal_aborts_without_poison_or_deletion() {
    for point in [
        journal::WritePoint::BeforeSeal,
        journal::WritePoint::AfterSeal,
    ] {
        probe("cancelled", move |group| async move {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            let mut index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
            index.recover().await.unwrap();
            let p = index
                .publish(prepared("p", "Alice", "first", None), &group)
                .await
                .unwrap();
            let once = AtomicBool::new(false);
            let stop = group.preparation_stop();
            index.journal.hook = Some(Arc::new(move |at, _| {
                if at == point && !once.swap(true, Ordering::SeqCst) {
                    stop.cancel();
                }
                Ok(())
            }));
            assert_eq!(index.reset(&group).await, Err(IndexError::Cancelled));
            assert_eq!(index.gate.readiness(), Readiness::Ready);
            assert_eq!(index.published_paper("p".into()).await.unwrap(), Some(p));
            Err(ErrorCode::Cancelled)
        })
        .await;
    }
}

#[tokio::test]
async fn journal_schema_lease_and_path_refusals_are_nonmutating() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
    index.recover().await.unwrap();
    assert!(matches!(
        CoordinatedIndex::open(&paths, identity()).await,
        Err(IndexError::Conflict)
    ));
    let mut aliases = paths.clone();
    aliases.index_state_database = paths.history_database.clone();
    assert!(matches!(
        CoordinatedIndex::open(&aliases, identity()).await,
        Err(IndexError::InvalidPath)
    ));
    index.graph.close().await.unwrap();
    drop(index);
    let db = sqlite::open(&paths.index_state_database).unwrap();
    db.execute("CREATE TABLE unexpected(x)").unwrap();
    drop(db);
    let before = std::fs::read(&paths.index_state_database).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            CoordinatedIndex::open(&paths, identity()).await,
            Err(IndexError::InvalidJournal)
        ));
    }
    assert_eq!(std::fs::read(&paths.index_state_database).unwrap(), before);
}

#[tokio::test]
async fn journal_corrupt_authority_and_unknown_retrieval_poison_without_writes() {
    for corruption in 0..4 {
        fixtures::with_group(move |group| async move {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
            index.recover().await.unwrap();
            index
                .publish(prepared("p", "Alice", "first", None), &group)
                .await
                .unwrap();
            if corruption == 0 {
                sqlite::open(&paths.index_state_database)
                    .unwrap()
                    .execute("UPDATE global_labels SET payload='{}'")
                    .unwrap();
            }
            if corruption == 1 {
                sqlite::open(&paths.index_state_database)
                    .unwrap()
                    .execute("DELETE FROM paper_node_memberships")
                    .unwrap();
            }
            if corruption == 2 {
                sqlite::open(&paths.index_state_database)
                    .unwrap()
                    .execute("UPDATE operations SET payload=payload || ' '")
                    .unwrap();
            }
            if corruption == 3 {
                index
                    .retrieval
                    .apply(
                        &[
                            PreparedOperation::DeletePaper("p".into()),
                            PreparedOperation::RebuildLexical,
                        ],
                        Some(1),
                        &Cancellation::default(),
                    )
                    .unwrap();
            }
            let before = std::fs::read(&paths.index_state_database).unwrap();
            let fence = index.fence().await.unwrap();
            assert!(index.published_paper("p".into()).await.is_err());
            assert_eq!(index.gate.readiness(), Readiness::Poisoned);
            assert!(index.recover().await.is_err());
            assert_eq!(index.fence().await.unwrap(), fence);
            assert_eq!(std::fs::read(&paths.index_state_database).unwrap(), before);
        })
        .await;
    }
}

#[tokio::test]
async fn journal_physical_singletons_reject_corruption_live_and_after_reopen() {
    for table in ["index_meta", "operations"] {
        for corruption in ["wrong_key", "extra_row", "blob_payload"] {
            for steps in [None, Some(0), Some(5)] {
                if table == "operations" && steps.is_none() {
                    continue;
                }
                for reopen_first in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let paths = paths(root.path());
                    let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
                    assert!(index.journal.meta().unwrap().is_none());
                    assert!(index.journal.operation().unwrap().is_none());
                    index.recover().await.unwrap();
                    if let Some(steps) = steps {
                        let op = operation(&index, prepared("p", "Alice", "first", None)).await;
                        stage(&index, &op, steps).await;
                    }
                    let sql = match corruption {
                        "wrong_key" => format!("UPDATE {table} SET id=2"),
                        "extra_row" => format!("INSERT INTO {table} SELECT 2,payload FROM {table}"),
                        _ => format!("UPDATE {table} SET payload=CAST(payload AS BLOB)"),
                    };
                    let db = sqlite::open(&paths.index_state_database).unwrap();
                    db.execute("PRAGMA ignore_check_constraints=ON").unwrap();
                    db.execute(sql).unwrap();
                    drop(db);
                    let before = std::fs::read(&paths.index_state_database).unwrap();
                    let fence = index.fence().await.unwrap();
                    let status = index.status().await.unwrap();
                    let index = if reopen_first {
                        reopen(index, &paths).await
                    } else {
                        index
                    };
                    if !reopen_first && steps != Some(0) {
                        assert_eq!(
                            index.published_paper("p".into()).await,
                            Err(IndexError::InvalidJournal)
                        );
                        assert_eq!(index.gate.readiness(), Readiness::Poisoned);
                    }
                    for _ in 0..2 {
                        assert_eq!(
                            index.recover().await,
                            Err(IndexError::InvalidJournal),
                            "{table}/{corruption}/{steps:?}/{reopen_first}"
                        );
                        assert_eq!(index.gate.readiness(), Readiness::Poisoned);
                        assert_eq!(
                            index.published_paper("p".into()).await,
                            Err(IndexError::Poisoned)
                        );
                        assert_eq!(index.fence().await.unwrap(), fence);
                        let observed = index.status().await.unwrap();
                        assert_eq!(observed.revision, status.revision);
                        assert_eq!(observed.lexical, status.lexical);
                        assert_eq!(observed.chunk_count, status.chunk_count);
                        assert_eq!(std::fs::read(&paths.index_state_database).unwrap(), before);
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn journal_singleton_loaders_reject_sqlite_type_coercion() {
    for table in ["index_meta", "operations"] {
        for (key, payload) in [
            ("'1'", "payload"),
            ("1.0", "payload"),
            ("X'31'", "payload"),
            ("NULL", "payload"),
            ("1", "NULL"),
            ("1", "42"),
            ("1", "1.5"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            let index = CoordinatedIndex::open(&paths, identity()).await.unwrap();
            index.recover().await.unwrap();
            let op = operation(&index, prepared("p", "Alice", "first", None)).await;
            stage(&index, &op, 0).await;
            // INTEGER PRIMARY KEY itself forbids noninteger stored keys. An
            // untyped live corruption fixture proves loaders do not coerce them.
            let db = sqlite::open(&paths.index_state_database).unwrap();
            db.execute(format!("BEGIN; ALTER TABLE {table} RENAME TO damaged; CREATE TABLE {table}(id,payload); INSERT INTO {table} SELECT {key},{payload} FROM damaged; DROP TABLE damaged; COMMIT")).unwrap();
            drop(db);
            let before = std::fs::read(&paths.index_state_database).unwrap();
            assert_eq!(index.journal.meta(), Err(IndexError::InvalidJournal));
            assert_eq!(index.journal.operation(), Err(IndexError::InvalidJournal));
            assert_eq!(index.recover().await, Err(IndexError::InvalidJournal));
            assert_eq!(index.gate.readiness(), Readiness::Poisoned);
            index.graph.close().await.unwrap();
            drop(index);
            assert!(matches!(
                CoordinatedIndex::open(&paths, identity()).await,
                Err(IndexError::InvalidJournal)
            ));
            assert_eq!(std::fs::read(&paths.index_state_database).unwrap(), before);
        }
    }
}
