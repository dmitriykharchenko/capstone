use super::*;
use crate::{
    history::{SqliteHistory, TurnRequest},
    providers::EmbeddingClient,
    retrieval::{NativeRetriever, PreparedExchangeArtifact},
};
struct Fake(Arc<AtomicUsize>);
impl EmbeddingClient for Fake {
    fn embed(&self, texts: Vec<String>, _: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(texts.iter().map(|_| vec![0.25, 0.75]).collect()) })
    }
}
fn history(paths: &StoragePaths) -> SqliteHistory {
    let h = SqliteHistory::new(&paths.data_root, &paths.history_database).unwrap();
    h.initialize(&Cancellation::default()).unwrap();
    h
}
fn request(id: usize) -> TurnRequest {
    TurnRequest {
        turn_id: format!("turn-{id}"),
        session_id: "s".into(),
        message: format!("question-{id}"),
    }
}
fn commit(h: &SqliteHistory, id: usize) -> crate::history::CommittedTurn {
    let r = request(id);
    h.admit_turn(&r, &Cancellation::default()).unwrap();
    h.append_turn(&r, "answer", &[], &Cancellation::default())
        .unwrap();
    h.get_turn_outcome(&r).unwrap().unwrap()
}
#[tokio::test]
async fn prepared_exchange_and_apply_before_marker_recover_without_provider_replay() {
    for applied in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let p = paths(root.path());
        let h = history(&p);
        let canonical = commit(&h, 1);
        let index = CoordinatedIndex::open(&p, identity())
            .await
            .unwrap()
            .with_history(h.clone());
        index.recover().await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let retriever = index.retriever(Arc::new(Fake(calls.clone())));
        let prepared = retriever
            .prepare_exchange(1, Some("s".into()), Cancellation::default())
            .await
            .unwrap();
        let artifact = PreparedExchangeArtifact::new(prepared, &canonical).unwrap();
        let meta = index.journal.meta().unwrap().unwrap();
        let op = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            base: meta.clone(),
            phase: Phase::Prepared,
            kind: Kind::Exchange(Box::new(journal::ExchangeEntry {
                intent: canonical.intent,
                epoch: meta.epoch,
                artifact: Some(artifact.clone()),
                complete: false,
            })),
        };
        index
            .journal
            .prepare(&op, &Cancellation::default())
            .unwrap();
        if applied {
            index
                .retrieval
                .apply(
                    &[
                        PreparedOperation::UpsertExchange(artifact.restore(&identity()).unwrap()),
                        PreparedOperation::RebuildLexical,
                    ],
                    Some(meta.revision),
                    &Cancellation::default(),
                )
                .unwrap();
        }
        index.graph.close().await.unwrap();
        drop(retriever);
        drop(index);
        let index = CoordinatedIndex::open(&p, identity())
            .await
            .unwrap()
            .with_history(h);
        index.recover().await.unwrap();
        assert!(index.observe_exchange(1).await.unwrap().indexed);
        let revision = index.status().await.unwrap().revision;
        index.recover().await.unwrap();
        assert_eq!(revision, index.status().await.unwrap().revision);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn startup_pages_canonical_intents_without_embeddings_and_reset_preserves_them() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let h = history(&p);
    for id in 1..=260 {
        commit(&h, id);
    }
    let index = CoordinatedIndex::open(&p, identity())
        .await
        .unwrap()
        .with_history(h.clone());
    index.recover().await.unwrap();
    assert_eq!(index.journal.scan_cursor().unwrap(), 260);
    assert!(!index.observe_exchange(260).await.unwrap().indexed);
    let before = h.get_messages("s").unwrap();
    let reset = index.clone();
    probe("result", move |group| async move {
        reset.reset(&group).await?;
        Ok(json!({}))
    })
    .await;
    assert_eq!(h.get_messages("s").unwrap().len(), before.len());
    assert_eq!(h.list_exchange_index_intents(0, 256).unwrap().len(), 256);
    index.recover().await.unwrap();
    assert_eq!(index.journal.scan_cursor().unwrap(), 260);
    assert_eq!(index.observe_exchange(1).await.unwrap().reset_epoch, 1);
}
#[tokio::test]
async fn exact_exchange_artifact_rejects_corruption_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let h = history(&p);
    let canonical = commit(&h, 1);
    let index = CoordinatedIndex::open(&p, identity())
        .await
        .unwrap()
        .with_history(h.clone());
    index.recover().await.unwrap();
    let retriever: NativeRetriever = index.retriever(Arc::new(Fake(Arc::new(AtomicUsize::new(0)))));
    let artifact = PreparedExchangeArtifact::new(
        retriever
            .prepare_exchange(1, Some("s".into()), Cancellation::default())
            .await
            .unwrap(),
        &canonical,
    )
    .unwrap();
    let mut value = serde_json::to_value(&artifact).unwrap();
    value["context"] = json!("corrupted context");
    let damaged: PreparedExchangeArtifact = serde_json::from_value(value).unwrap();
    assert!(damaged.verify(&canonical).is_err());
    let db = sqlite::open(&p.history_database).unwrap();
    db.execute("UPDATE messages SET content='changed' WHERE role='assistant'")
        .unwrap();
    drop(db);
    assert!(h.get_turn_outcome(&request(1)).is_err());
    let before = std::fs::read(&p.vector_database).unwrap();
    assert!(index.recover().await.is_err());
    assert_eq!(index.gate.readiness(), Readiness::Poisoned);
    assert_eq!(std::fs::read(&p.vector_database).unwrap(), before);
}
