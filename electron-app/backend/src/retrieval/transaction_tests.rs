use super::*;
use crate::retrieval::{PreparedPaper, PreparedReset};
use std::{sync::mpsc, time::Duration};

fn token() -> Cancellation {
    Cancellation::default()
}
fn setup() -> (tempfile::TempDir, VectorStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = VectorStore::new(
        dir.path(),
        &dir.path().join("vectors.sqlite"),
        EmbeddingIdentity {
            model: "fake".into(),
            dimension: 1,
        },
    )
    .unwrap();
    s.initialize(&token()).unwrap();
    s.apply(&[paper(&s, "old")], None, &token()).unwrap();
    (dir, s)
}
fn paper(s: &VectorStore, text: &str) -> PreparedOperation {
    PreparedOperation::ReplacePaper(
        PreparedPaper::new(
            "p".into(),
            s.identity.clone(),
            vec![EmbeddedChunk {
                id: "p:0".into(),
                paper_id: "p".into(),
                chunk_index: 0,
                filename: "p.pdf".into(),
                title: "P".into(),
                page: 1,
                text: text.into(),
                vector: vec![0.0],
            }],
        )
        .unwrap(),
    )
}
fn assert_old(s: &VectorStore) {
    let reopen = VectorStore::new(&s.root, &s.path, s.identity.clone()).unwrap();
    assert_eq!(reopen.snapshot(&token()).unwrap().chunks[0].text, "old");
}
#[test]
fn retrieval_cancellation_after_staging_rolls_back_replacement_reset_and_lexical() {
    let (_dir, s) = setup();
    let before = s.status(&token()).unwrap();
    for ops in [
        vec![paper(&s, "new"), PreparedOperation::RebuildLexical],
        vec![PreparedOperation::Reset(
            PreparedReset::new(s.identity.clone()).unwrap(),
        )],
        vec![PreparedOperation::DeletePaper("p".into())],
    ] {
        let c = token();
        let result = s.apply_inner(&ops, None, &c, |_| {
            assert!(c.cancel());
            Ok(())
        });
        assert_eq!(result, Err(RetrievalError::Cancelled));
        assert_eq!(s.status(&token()).unwrap(), before);
        assert_old(&s);
    }
}
#[test]
fn retrieval_failure_and_panic_after_staging_roll_back() {
    let (_dir, s) = setup();
    let c = token();
    assert_eq!(
        s.apply_inner(&[paper(&s, "new")], None, &c, |_| Err(
            RetrievalError::Storage
        )),
        Err(RetrievalError::Storage)
    );
    assert!(c.cancel());
    assert_old(&s);
    let c = token();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        s.apply_inner(&[paper(&s, "new")], None, &c, |_| {
            panic!("controlled before-commit failure")
        })
    }));
    assert!(panic.is_err());
    assert!(c.cancel());
    assert_old(&s);
}
#[test]
fn retrieval_actual_sqlite_commit_failure_rolls_back_but_token_stays_sealed() {
    let (_dir, s) = setup();
    let reader = s.connection(false).unwrap();
    reader.execute("BEGIN").unwrap();
    query(&reader, "SELECT payload FROM chunks", &[], &token()).unwrap();
    let c = token();
    // A real SQLite reader lock permits staging but forces COMMIT to return BUSY.
    let result = s.apply_inner(&[paper(&s, "new")], None, &c, |db| {
        db.execute("PRAGMA busy_timeout=0")?;
        Ok(())
    });
    assert_eq!(result, Err(RetrievalError::Storage));
    assert!(!c.cancel());
    drop(reader);
    assert_old(&s);
    assert_eq!(
        s.apply(&[paper(&s, "retry")], None, &c),
        Err(RetrievalError::Conflict)
    );
    assert_old(&s);
}
#[test]
fn retrieval_concurrent_reader_sees_old_or_new_complete_snapshot_never_staged_data() {
    let (_dir, s) = setup();
    let writer = s.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        writer.apply_inner(
            &[paper(&writer, "new"), PreparedOperation::RebuildLexical],
            None,
            &token(),
            |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(())
            },
        )
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_old(&s);
    release_tx.send(()).unwrap();
    assert_eq!(
        thread.join().unwrap().unwrap().lexical,
        LexicalStatus::Ready
    );
    let after = s.snapshot(&token()).unwrap();
    assert_eq!(after.chunks[0].text, "new");
    assert_eq!(after.status.lexical, LexicalStatus::Ready);
}
#[test]
fn retrieval_content_fingerprint_rejects_same_revision_stale_or_malformed_lexical_statistics() {
    let (_dir, s) = setup();
    s.apply(&[PreparedOperation::RebuildLexical], None, &token())
        .unwrap();
    let db = sqlite::open(&s.path).unwrap();
    let rows = query(&db, "SELECT payload FROM lexical", &[], &token()).unwrap();
    let mut payload: serde_json::Value = serde_json::from_str(text(&rows[0][0]).unwrap()).unwrap();
    payload["index"]["lengths"] = serde_json::json!([900]);
    execute(
        &db,
        "UPDATE lexical SET payload=?",
        &[payload.to_string().into()],
    )
    .unwrap();
    assert_eq!(s.status(&token()).unwrap().lexical, LexicalStatus::Stale);
    // Simulate an out-of-band same-count replacement with an unchanged revision.
    s.apply(&[PreparedOperation::RebuildLexical], None, &token())
        .unwrap();
    let rows = query(&db, "SELECT payload FROM chunks", &[], &token()).unwrap();
    let mut chunk: EmbeddedChunk = decode(&rows[0][0]).unwrap();
    chunk.text = "different".into();
    execute(
        &db,
        "UPDATE chunks SET payload=?",
        &[encode(&chunk).unwrap().into()],
    )
    .unwrap();
    let snapshot = s.snapshot(&token()).unwrap();
    assert_eq!(snapshot.status.lexical, LexicalStatus::Stale);
    assert_eq!(snapshot.lexical.score("old", &token()).unwrap(), vec![0.0]);
}
#[test]
fn retrieval_malformed_corpus_or_future_schema_fails_closed_without_repair() {
    let (_dir, s) = setup();
    let db = sqlite::open(&s.path).unwrap();
    db.execute("UPDATE chunks SET payload='{}'").unwrap();
    assert_eq!(
        s.snapshot(&token()).unwrap_err(),
        RetrievalError::InvalidStore
    );
    assert_eq!(
        s.apply(
            &[PreparedOperation::Reset(
                PreparedReset::new(s.identity.clone()).unwrap()
            )],
            None,
            &token()
        ),
        Err(RetrievalError::InvalidStore)
    );
    assert_eq!(
        text(&query(&db, "SELECT payload FROM chunks", &[], &token()).unwrap()[0][0]).unwrap(),
        "{}"
    );
    db.execute("PRAGMA user_version=999").unwrap();
    assert_eq!(s.initialize(&token()), Err(RetrievalError::InvalidStore));
}
#[test]
fn retrieval_cancelled_initialization_never_publishes_and_existing_store_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let s = VectorStore::new(
        dir.path(),
        &dir.path().join("v.sqlite"),
        EmbeddingIdentity {
            model: "fake".into(),
            dimension: 1,
        },
    )
    .unwrap();
    let c = token();
    c.cancel();
    assert_eq!(s.initialize(&c), Err(RetrievalError::Cancelled));
    assert!(!s.path.exists());
    let c = token();
    s.initialize(&c).unwrap();
    assert!(!c.cancel());
    s.apply(&[paper(&s, "old")], None, &token()).unwrap();
    let bytes = fs::read(&s.path).unwrap();
    s.initialize(&token()).unwrap();
    assert_eq!(fs::read(&s.path).unwrap(), bytes);
}
