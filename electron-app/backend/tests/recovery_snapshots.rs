pub use research_backend::{registry, runtime};
mod recovery_fixtures;
use recovery_fixtures::with_group;
use research_backend::{graph as g, providers::EmbeddingClient, retrieval as r};
use runtime::Cancellation;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn identity() -> r::EmbeddingIdentity {
    r::EmbeddingIdentity {
        model: "fake/model/mode".into(),
        dimension: 2,
    }
}
fn chunk(id: &str, index: u32, text: &str) -> r::EmbeddedChunk {
    r::EmbeddedChunk {
        id: id.into(),
        paper_id: "p".into(),
        chunk_index: index,
        filename: "p.pdf".into(),
        title: "Title".into(),
        page: index + 1,
        text: text.into(),
        vector: vec![0.25, -0.0],
    }
}
fn prepared(text: &str) -> r::PreparedPaper {
    r::PreparedPaper::new(
        "p".into(),
        identity(),
        vec![chunk("p:1", 1, "Second"), chunk("p:0", 0, text)],
    )
    .unwrap()
}
fn graph(id: &str, title: &str, name: &str) -> g::PreparedPaper {
    g::PreparedPaper::new(
        id,
        title,
        &[name.into(), "Zed".into()],
        &["RAG".into(), "Vectors".into()],
    )
    .unwrap()
}
struct NoEmbedding(Arc<AtomicUsize>);
impl EmbeddingClient for NoEmbedding {
    fn embed(&self, _: Vec<String>, _: Cancellation) -> registry::BackendFuture<Vec<Vec<f64>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { panic!("restore must not call provider") })
    }
}

#[tokio::test]
async fn retrieval_recovery_restores_finite_vector_bits_through_actual_store() {
    with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("v.sqlite");
        let store = r::VectorStore::new(root.path(), &path, identity()).unwrap();
        store.initialize(&Cancellation::default()).unwrap();
        let mut chunks = Vec::new();
        let mut bits = 0x123456789abcdef0u64;
        for index in 0..256 {
            bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
            let value = f64::from_bits(bits);
            if value.is_finite() {
                let mut c = chunk(&format!("p:{index}"), index, "vector");
                c.vector = vec![value, -value];
                chunks.push(c);
            }
        }
        for (offset, value) in [
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::MIN,
            1.2345678901234567,
        ]
        .into_iter()
        .enumerate()
        {
            let index = 256 + offset as u32;
            let mut c = chunk(&format!("p:{index}"), index, "edge vector");
            c.vector = vec![value, -value];
            chunks.push(c);
        }
        let desired = r::PaperRecoveryArtifact::from_prepared(
            &r::PreparedPaper::new("p".into(), identity(), chunks).unwrap(),
        )
        .unwrap();
        drop(store);
        for _ in 0..3 {
            let store = r::VectorStore::new(root.path(), &path, identity()).unwrap();
            let restored =
                r::PaperRecoveryArtifact::from_json(&desired.to_json().unwrap()).unwrap();
            let op = restored.restore_operation(&identity()).unwrap();
            let s = store.clone();
            group
                .run_mutation("restore bits", move |c| async move {
                    s.apply(&[op], None, &c).map_err(Into::into)
                })
                .await
                .unwrap();
            let actual = store
                .observe_paper("p", &group.preparation_stop())
                .unwrap()
                .artifact;
            assert_eq!(actual.digest(), desired.digest());
            assert!(actual == desired, "exact vector payload changed");
            drop(store);
            let reopened = r::VectorStore::new(root.path(), &path, identity()).unwrap();
            let actual = reopened
                .observe_paper("p", &group.preparation_stop())
                .unwrap()
                .artifact;
            assert_eq!(actual.digest(), desired.digest());
            assert!(actual == desired, "reopened vector payload changed");
        }
    })
    .await;
}

#[test]
fn retrieval_artifact_roundtrip_canonical_order_bits_and_all_fields_digest() {
    let original = r::PaperRecoveryArtifact::from_prepared(&prepared("First")).unwrap();
    let wire = original.to_json().unwrap();
    assert_eq!(
        original,
        r::PaperRecoveryArtifact::from_json(&wire).unwrap()
    );
    let r::PreparedOperation::ReplacePaper(restored) =
        original.restore_operation(&identity()).unwrap()
    else {
        panic!()
    };
    assert_eq!(restored.chunks()[0].chunk_index, 0);
    assert_eq!(
        restored.chunks()[0].vector[1].to_bits(),
        (-0.0f64).to_bits()
    );
    let mut chunks = restored.chunks().to_vec();
    chunks.reverse();
    assert_eq!(
        original,
        r::PaperRecoveryArtifact::from_prepared(
            &r::PreparedPaper::new("p".into(), identity(), chunks).unwrap()
        )
        .unwrap()
    );
    for change in 0..10 {
        let mut id = "p".to_owned();
        let mut identity = identity();
        let mut chunks = restored.chunks().to_vec();
        match change {
            0 => chunks[0].id = "other-id".into(),
            1 => chunks[0].chunk_index = 3,
            2 => chunks[0].filename = "other.pdf".into(),
            3 => chunks[0].title = "Other".into(),
            4 => chunks[0].page = 9,
            5 => chunks[0].text = "Other".into(),
            6 => chunks[0].vector[0] = 0.5,
            7 => identity.model = "other-model".into(),
            8 => {
                id = "q".into();
                for c in &mut chunks {
                    c.paper_id = id.clone();
                }
            }
            _ => {
                identity.dimension = 1;
                for c in &mut chunks {
                    c.vector.pop();
                }
            }
        }
        let changed = r::PaperRecoveryArtifact::from_prepared(
            &r::PreparedPaper::new(id, identity, chunks).unwrap(),
        )
        .unwrap();
        assert_ne!(original.digest(), changed.digest());
    }
    for value in [
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::from_bits(1),
        1.2345678901234567,
    ] {
        let mut c = chunk("p:0", 0, "text");
        c.vector = vec![value, -value];
        let artifact = r::PaperRecoveryArtifact::from_prepared(
            &r::PreparedPaper::new("p".into(), identity(), vec![c]).unwrap(),
        )
        .unwrap();
        assert_eq!(
            artifact,
            r::PaperRecoveryArtifact::from_json(&artifact.to_json().unwrap()).unwrap()
        );
    }
}

#[test]
fn retrieval_artifact_rejects_malformed_tampered_unsupported_and_oversized() {
    let artifact = r::PaperRecoveryArtifact::from_prepared(&prepared("First")).unwrap();
    let raw = String::from_utf8(artifact.to_json().unwrap()).unwrap();
    for (old, new) in [
        ("\"version\":1", "\"version\":1,\"version\":1"),
        ("\"model\":", "\"extra\":0,\"model\":"),
        ("\"id\":", "\"id\":\"dup\",\"id\":"),
        ("\"body\":", "\"extra\":0,\"body\":"),
        ("\"dimension\":2", "\"dimension\":2,\"dimension\":2"),
    ] {
        assert!(r::PaperRecoveryArtifact::from_json(raw.replacen(old, new, 1).as_bytes()).is_err());
    }
    let base: Value = serde_json::from_str(&raw).unwrap();
    for (path, value) in [
        ("/body/version", json!(2)),
        ("/body/identity/dimension", json!(0)),
        ("/body/identity/dimension", json!(1)),
        ("/body/chunks/0/page", json!(0)),
        ("/body/chunks/0/chunk_index", json!(-1)),
        ("/body/chunks/0/chunk_index", json!(4294967296u64)),
        ("/body/chunks/0/paper_id", json!("q")),
        ("/body/chunks/0/id", json!("")),
        ("/body/chunks/0/vector_bits/0", json!("7ff0000000000000")),
        ("/body/chunks/0/vector_bits/0", json!("7ff8000000000000")),
        ("/body/chunks/0/vector_bits/0", json!("BFD0000000000000")),
        ("/body/chunks/0/vector_bits/0", json!(42)),
        ("/body/chunks/0/text", json!("tampered")),
        ("/body/chunks/1/id", json!("p:0")),
        ("/body/chunks/1/chunk_index", json!(0)),
        ("/body/chunks/0/text", json!("x".repeat(8001))),
        ("/sha256", json!("bad")),
    ] {
        let mut v = base.clone();
        *v.pointer_mut(path).unwrap() = value;
        assert!(
            r::PaperRecoveryArtifact::from_json(&serde_json::to_vec(&v).unwrap()).is_err(),
            "accepted {path}"
        );
    }
    assert_eq!(
        r::PaperRecoveryArtifact::from_json(&vec![b' '; r::MAX_PAPER_ARTIFACT_BYTES + 1]),
        Err(r::RetrievalError::ArtifactTooLarge)
    );
    assert!(
        artifact
            .restore_operation(&r::EmbeddingIdentity {
                model: "other".into(),
                dimension: 2
            })
            .is_err()
    );
    assert!(r::PaperRecoveryArtifact::from_json(b"{}").is_err());
}

#[test]
fn graph_artifact_strict_canonical_roundtrip_and_digest() {
    let artifact = g::PaperRecoveryArtifact::from_prepared(&graph("p", "Title", "Alice")).unwrap();
    let raw = String::from_utf8(artifact.to_json().unwrap()).unwrap();
    assert_eq!(
        artifact,
        g::PaperRecoveryArtifact::from_json(raw.as_bytes()).unwrap()
    );
    let base: Value = serde_json::from_str(&raw).unwrap();
    for (path, value) in [
        ("/body/version", json!(2)),
        ("/body/paper_id", json!("q")),
        ("/body/paper/title", json!(" Title ")),
        ("/body/paper/authors/0/author_id", json!("author:bad")),
        ("/body/paper/authors/0/name", json!("Ａlice")),
        ("/body/paper/topics/0/topic_id", json!("topic:bad")),
        ("/body/paper/authors", json!([])),
        ("/body/paper/topics", json!([])),
        ("/body/paper/title", json!("x".repeat(4097))),
        ("/sha256", json!("bad")),
    ] {
        let mut v = base.clone();
        *v.pointer_mut(path).unwrap() = value;
        assert!(
            g::PaperRecoveryArtifact::from_json(&serde_json::to_vec(&v).unwrap()).is_err(),
            "accepted {path}"
        );
    }
    for duplicate in [
        "version",
        "paper_id",
        "title",
        "name",
        "author_id",
        "topic_id",
    ] {
        let find = format!("\"{duplicate}\":");
        let bad = raw.replacen(&find, &format!("\"{duplicate}\":null,{find}"), 1);
        assert!(g::PaperRecoveryArtifact::from_json(bad.as_bytes()).is_err());
    }
    let mut v = base.clone();
    v["body"]["paper"]["authors"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert!(g::PaperRecoveryArtifact::from_json(&serde_json::to_vec(&v).unwrap()).is_err());
    v = base.clone();
    v["body"]["paper"]["extra"] = json!(true);
    assert!(g::PaperRecoveryArtifact::from_json(&serde_json::to_vec(&v).unwrap()).is_err());
    v = base;
    v["body"].as_object_mut().unwrap().remove("paper");
    assert!(g::PaperRecoveryArtifact::from_json(&serde_json::to_vec(&v).unwrap()).is_err());
    for changed in [
        graph("q", "Title", "Alice"),
        graph("p", "Other", "Alice"),
        graph("p", "Title", "ALICE"),
        graph("p", "Title", "Bob"),
    ] {
        assert_ne!(
            artifact.digest(),
            g::PaperRecoveryArtifact::from_prepared(&changed)
                .unwrap()
                .digest()
        );
    }
    assert_eq!(
        g::PaperRecoveryArtifact::from_json(&vec![b' '; g::MAX_GRAPH_ARTIFACT_BYTES + 1]),
        Err(g::GraphError::ArtifactTooLarge)
    );
    assert!(g::PaperRecoveryArtifact::absent("author:x".into()).is_err());
    let absent = g::PaperRecoveryArtifact::absent("p".into()).unwrap();
    assert_ne!(artifact, absent);
    assert_eq!(
        absent,
        g::PaperRecoveryArtifact::from_json(&absent.to_json().unwrap()).unwrap()
    );
}

#[tokio::test]
async fn retrieval_local_restore_reopen_no_rows_lexical_and_zero_provider_calls() {
    with_group(|group| async move {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("retrieval.sqlite");
        let store = r::VectorStore::new(root.path(), &path, identity()).unwrap();
        store.initialize(&Cancellation::default()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service =
            r::NativeRetriever::new(store.clone(), Arc::new(NoEmbedding(calls.clone())), None);
        let absent = service
            .observe_paper("p".into(), group.preparation_stop())
            .await
            .unwrap();
        assert_eq!(absent.revision, 0);
        assert_eq!(absent.lexical, r::LexicalStatus::Stale);
        assert!(!absent.artifact.has_rows());
        let desired = r::PaperRecoveryArtifact::from_prepared(&prepared("First")).unwrap();
        for text in ["First", "Other", "First", "First"] {
            let artifact = r::PaperRecoveryArtifact::from_prepared(&prepared(text)).unwrap();
            let operation = artifact.restore_operation(&identity()).unwrap();
            let service = service.clone();
            group
                .run_mutation("restore", move |c| async move {
                    service
                        .apply(
                            vec![operation, r::PreparedOperation::RebuildLexical],
                            None,
                            c,
                        )
                        .await
                        .map_err(Into::into)
                })
                .await
                .unwrap();
            let actual = store.observe_paper("p", &group.preparation_stop()).unwrap();
            assert_eq!(actual.artifact, artifact);
            assert_eq!(actual.lexical, r::LexicalStatus::Ready);
        }
        drop(service);
        drop(store);
        let reopened = r::VectorStore::new(root.path(), &path, identity()).unwrap();
        assert_eq!(
            reopened
                .observe_paper("p", &group.preparation_stop())
                .unwrap()
                .artifact,
            desired
        );
        for _ in 0..2 {
            let operation = absent.artifact.restore_operation(&identity()).unwrap();
            let s = reopened.clone();
            group
                .run_mutation("remove", move |c| async move {
                    s.apply(&[operation, r::PreparedOperation::RebuildLexical], None, &c)
                        .map_err(Into::into)
                })
                .await
                .unwrap();
            let observed = reopened
                .observe_paper("p", &group.preparation_stop())
                .unwrap();
            assert_eq!(observed.artifact, absent.artifact);
            assert_eq!(observed.lexical, r::LexicalStatus::Ready);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    })
    .await;
}

#[tokio::test]
async fn graph_restore_shared_labels_edges_close_reopen_and_repeated_removal() {
    with_group(|group| async move {
        for author_collision in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("graph.sqlite");
            let store =
                g::CozoGraphStore::open(root.path().into(), path.clone(), Cancellation::default())
                    .await
                    .unwrap();
            let absent = store
                .observe_paper("p".into(), group.preparation_stop())
                .await
                .unwrap();
            assert!(absent.paper().is_none());
            let desired =
                g::PaperRecoveryArtifact::from_prepared(&graph("p", "Title", "Alice")).unwrap();
            let s = store.clone();
            let artifact = desired.clone();
            group
                .run_mutation("isolated restore", move |c| async move {
                    s.restore_paper(artifact, c).await.map_err(Into::into)
                })
                .await
                .unwrap();
            let other = g::PreparedPaper::new(
                "q",
                "Other",
                &[
                    if author_collision {
                        "ALICE".into()
                    } else {
                        "Alice".into()
                    },
                    "Zed".into(),
                ],
                &[
                    if author_collision {
                        "RAG".into()
                    } else {
                        "rag".into()
                    },
                    "Vectors".into(),
                ],
            )
            .unwrap();
            let s = store.clone();
            group
                .run_mutation("legacy shared overwrite", move |c| async move {
                    s.apply(vec![g::GraphOperation::Replace(other)], c)
                        .await
                        .map_err(Into::into)
                })
                .await
                .unwrap();
            let changed = store
                .observe_paper("p".into(), group.preparation_stop())
                .await
                .unwrap();
            assert_ne!(changed.digest(), desired.digest());
            let q = store
                .observe_paper("q".into(), group.preparation_stop())
                .await
                .unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let status = store.status(group.preparation_stop()).await.unwrap();
            for _ in 0..2 {
                let s = store.clone();
                let artifact = desired.clone();
                let result = group
                    .run_mutation("refuse conflicting restore", move |c| async move {
                        Ok(s.restore_paper(artifact, c).await)
                    })
                    .await
                    .unwrap();
                assert_eq!(result, Err(g::GraphError::SharedNodeConflict));
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert_eq!(
                    store.status(group.preparation_stop()).await.unwrap(),
                    status
                );
                assert_eq!(
                    store
                        .observe_paper("p".into(), group.preparation_stop())
                        .await
                        .unwrap(),
                    changed
                );
                assert_eq!(
                    store
                        .observe_paper("q".into(), group.preparation_stop())
                        .await
                        .unwrap(),
                    q
                );
            }
            for _ in 0..2 {
                let s = store.clone();
                let artifact =
                    g::PaperRecoveryArtifact::from_json(&changed.to_json().unwrap()).unwrap();
                group
                    .run_mutation("graph restore", move |c| async move {
                        s.restore_paper(artifact, c).await.map_err(Into::into)
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    store
                        .observe_paper("p".into(), group.preparation_stop())
                        .await
                        .unwrap(),
                    changed
                );
            }
            assert_eq!(
                store
                    .observe_paper("q".into(), group.preparation_stop())
                    .await
                    .unwrap(),
                q
            );
            assert_eq!(q.paper().unwrap().title, "Other");
            store.close().await.unwrap();
            let reopened =
                g::CozoGraphStore::open(root.path().into(), path, Cancellation::default())
                    .await
                    .unwrap();
            assert_eq!(
                reopened
                    .observe_paper("p".into(), group.preparation_stop())
                    .await
                    .unwrap(),
                changed
            );
            for _ in 0..2 {
                let s = reopened.clone();
                let artifact = absent.clone();
                group
                    .run_mutation("graph remove", move |c| async move {
                        s.restore_paper(artifact, c).await.map_err(Into::into)
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    reopened
                        .observe_paper("p".into(), group.preparation_stop())
                        .await
                        .unwrap(),
                    absent
                );
                assert_eq!(
                    reopened
                        .observe_paper("q".into(), group.preparation_stop())
                        .await
                        .unwrap(),
                    q
                );
            }
            reopened.close().await.unwrap();
        }
    })
    .await;
}
