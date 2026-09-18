//! Native fixtures ported from tests/test_paper_search.py (current working tree),
//! tests/test_bm25_index.py, tests/test_chat_history_index.py and
//! src/bm25.py. No Python, Chroma, legacy paths or real providers are used.
use research_backend::{
    domain::{Chunk, RetrievalScope as Scope, RetrievalStrategy as Strategy, SourceRecord},
    history::SqliteHistory,
    protocol::ErrorCode,
    providers::EmbeddingClient,
    registry::BackendFuture,
    retrieval::{
        EmbeddedChunk, EmbeddingIdentity, HistorySearchRequest, LexicalStatus, NativeRetriever,
        PreparedOperation as Op, PreparedPaper, PreparedReset, RetrievalError as E, Retriever,
        SearchRequest, VectorStore, bm25_scores, hybrid_scores, similarity, squared_l2, tokenize,
    },
    runtime::Cancellation,
};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn token() -> Cancellation {
    Cancellation::default()
}
fn identity() -> EmbeddingIdentity {
    EmbeddingIdentity {
        model: "fake/test-v1".into(),
        dimension: 1,
    }
}
fn store() -> (TempDir, VectorStore) {
    let temp = TempDir::new().unwrap();
    let store =
        VectorStore::new(temp.path(), &temp.path().join("vectors.sqlite"), identity()).unwrap();
    store.initialize(&token()).unwrap();
    (temp, store)
}
fn record(paper: &str, index: u32, text: &str, distance: f64) -> EmbeddedChunk {
    EmbeddedChunk {
        id: format!("{paper}:{index}"),
        paper_id: paper.into(),
        chunk_index: index,
        filename: format!("{paper}.pdf"),
        title: format!("Paper {paper}"),
        page: index + 1,
        text: text.into(),
        vector: vec![distance.sqrt()],
    }
}
fn replace(store: &VectorStore, records: Vec<EmbeddedChunk>) {
    let ops = records
        .into_iter()
        .map(|r| {
            Op::ReplacePaper(
                PreparedPaper::new(r.paper_id.clone(), store.identity().clone(), vec![r]).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    store.apply(&ops, None, &token()).unwrap();
}
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    vector: Vec<f64>,
    fail: bool,
}
impl Fake {
    fn zero() -> Arc<Self> {
        Arc::new(Self {
            vector: vec![0.0],
            ..Self::default()
        })
    }
}
impl EmbeddingClient for Fake {
    fn embed(&self, texts: Vec<String>, c: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
        self.calls.lock().unwrap().push(texts.clone());
        let vector = self.vector.clone();
        let fail = self.fail;
        Box::pin(async move {
            c.check()?;
            if fail {
                Err(ErrorCode::Internal)
            } else {
                Ok(vec![vector; texts.len()])
            }
        })
    }
}
fn service(store: &VectorStore) -> NativeRetriever {
    NativeRetriever::new(store.clone(), Fake::zero(), None)
}
fn request(query: &str, strategy: Strategy, scope: Scope, limit: usize) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        strategy,
        scope,
        limit,
    }
}
fn ids(chunks: &[Chunk]) -> Vec<&str> {
    chunks.iter().map(|c| c.source.paper_id.as_str()).collect()
}

#[test]
fn metric_is_squared_l2_not_cosine_and_mapping_rounds_four_decimals() {
    // Chroma default is squared L2; open_collection passes no metric override.
    assert_eq!(squared_l2(&[1.0, 0.0], &[3.0, 0.0]).unwrap(), 4.0);
    assert_eq!(similarity(4.0).unwrap(), 0.2);
    assert_eq!(similarity(0.1).unwrap(), 0.9091);
    assert_eq!(similarity(0.10001).unwrap(), 0.9091);
    for d in [-1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(similarity(d), Err(E::InvalidInput));
    }
    assert_eq!(squared_l2(&[1.0], &[1.0, 2.0]), Err(E::ReindexRequired));
    assert_eq!(squared_l2(&[f64::MAX], &[-f64::MAX]), Err(E::InvalidInput));
}
#[test]
fn ascii_tokenizer_after_unicode_lowercase_and_bm25_query_dedup() {
    assert_eq!(
        tokenize("İX KELVIN Straße ÉCOLE Ａbc 42_under—A\u{1f}B"),
        vec![
            "i", "x", "kelvin", "stra", "e", "cole", "bc", "42", "under", "a", "b"
        ]
    );
    let docs = ["a a", "a b", "c"];
    let scores = bm25_scores("a a", &docs, &token()).unwrap();
    assert_eq!(scores, bm25_scores("a", &docs, &token()).unwrap());
    // Independently evaluated N=3, df(a)=2, avgdl=5/3, k1=1.5,b=.75.
    assert!((scores[0] - 0.6308773546922626).abs() < 1e-12);
    assert!((scores[1] - 0.43119599013370247).abs() < 1e-12);
    assert_eq!(scores[2], 0.0);
    assert_eq!(bm25_scores("a", &[], &token()).unwrap(), Vec::<f64>::new());
    assert_eq!(bm25_scores("", &docs, &token()).unwrap(), vec![0.0; 3]);
    assert_eq!(
        bm25_scores("a", &["", "!!!"], &token()).unwrap(),
        vec![0.0; 2]
    );
}
#[test]
fn hybrid_normalization_all_equal_zero_lexical_and_rounding() {
    assert_eq!(
        hybrid_scores(&[0.9524, 0.7143, 0.5556], &[0.0, 2.0, 0.0]).unwrap(),
        vec![0.5, 0.7, 0.0]
    );
    assert_eq!(
        hybrid_scores(&[0.3, 0.3], &[0.0, 0.0]).unwrap(),
        vec![0.5, 0.5]
    );
    assert_eq!(
        hybrid_scores(&[0.3, 0.3], &[1.0, 2.0]).unwrap(),
        vec![0.75, 1.0]
    );
    assert_eq!(hybrid_scores(&[], &[]).unwrap(), Vec::<f64>::new());
    assert!(hybrid_scores(&[1.0], &[]).is_err());
}
#[tokio::test]
async fn hybrid_parity_preserves_text_provenance_and_lexical_resurfacing() {
    // test_retrieve_chunks_hybrid_ranks_strong_lexical_match_above_vector.
    let (_temp, s) = store();
    replace(
        &s,
        vec![
            record("paper-a", 0, "graph neural network survey", 0.05),
            record(
                "paper-b",
                1,
                "transformer attention transformer attention model",
                0.4,
            ),
            record("paper-c", 2, "clustering methods overview", 0.8),
        ],
    );
    let result = service(&s)
        .retrieve(
            "transformer attention".into(),
            Strategy::Hybrid,
            Scope::ChatCandidates,
            5,
            token(),
        )
        .await
        .unwrap();
    assert_eq!(ids(&result), ["paper-b", "paper-a", "paper-c"]);
    assert_eq!(
        result.iter().map(|c| c.source.score).collect::<Vec<_>>(),
        [0.7, 0.5, 0.0]
    );
    assert_eq!(
        result.iter().map(|c| c.source.rank).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        (result[0].source.chunk_index, result[0].source.page),
        (1, 2)
    );
    assert_eq!(result[0].source.filename, "paper-b.pdf");
    assert_eq!(
        result[0].text,
        "transformer attention transformer attention model"
    );
}
#[tokio::test]
async fn scopes_use_distinct_ties_and_preserve_close_vector_order() {
    // test_hybrid_fusion_has_deterministic_provenance_tie_breaking and
    // test_vector_evaluation_preserves_chroma_order_for_close_distances.
    let (_temp, s) = store();
    replace(
        &s,
        vec![record("z", 0, "other", 0.1), record("a", 0, "needle", 0.5)],
    );
    let svc = service(&s);
    let chat = svc
        .retrieve_typed(
            request("needle", Strategy::Hybrid, Scope::ChatCandidates, 5),
            token(),
        )
        .await
        .unwrap();
    let full = svc
        .retrieve_typed(
            request("needle", Strategy::Hybrid, Scope::FullCorpus, 5),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(ids(&chat), ["z", "a"]);
    assert_eq!(ids(&full), ["a", "z"]);
    assert_eq!(chat[0].source.score, 0.5);
    assert_eq!(chat[1].source.score, 0.5);
    replace(&s, vec![record("a", 0, "second", 0.10001)]);
    let vector = svc
        .retrieve_typed(
            request("q", Strategy::Vector, Scope::FullCorpus, 5),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(ids(&vector), ["z", "a"]);
    assert_eq!(vector[0].source.score, vector[1].source.score);
}
#[tokio::test]
async fn candidate_budget_full_corpus_cap_and_no_provider_for_bm25() {
    let (_temp, s) = store();
    replace(
        &s,
        (0..60)
            .map(|i| {
                record(
                    &format!("p{i:02}"),
                    0,
                    if i == 59 { "needle" } else { "other" },
                    i as f64,
                )
            })
            .collect(),
    );
    let fake = Fake::zero();
    let svc = NativeRetriever::new(s.clone(), fake.clone(), None);
    let chat = svc
        .retrieve_typed(
            request("needle", Strategy::Hybrid, Scope::ChatCandidates, 100),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(chat.len(), 50);
    assert!(!ids(&chat).contains(&"p59"));
    let small = svc
        .retrieve_typed(
            request("needle", Strategy::Hybrid, Scope::ChatCandidates, 1),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(small.len(), 1);
    let calls = fake.calls.lock().unwrap().len();
    let lexical = svc
        .retrieve_typed(
            request("needle", Strategy::Bm25, Scope::FullCorpus, 100),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(lexical.len(), 5);
    assert_eq!(lexical[0].source.paper_id, "p59");
    assert_eq!(fake.calls.lock().unwrap().len(), calls);
    let full = svc
        .retrieve_typed(
            request("needle", Strategy::Hybrid, Scope::FullCorpus, 100),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(full.len(), 5);
    assert!(ids(&full).contains(&"p59"));
}
#[tokio::test]
async fn grouping_keeps_best_chunk_and_first_paper_tie_order_and_unicode_snippets() {
    let (_temp, s) = store();
    let long = format!("  {}\n\t tail", "é".repeat(245));
    let chunks = [
        record("p", 0, "no match", 0.01),
        record("p", 1, &long, 0.3),
        record("q", 0, "other", 0.5),
    ];
    s.apply(
        &[
            Op::ReplacePaper(
                PreparedPaper::new("p".into(), identity(), chunks[..2].to_vec()).unwrap(),
            ),
            Op::ReplacePaper(
                PreparedPaper::new("q".into(), identity(), vec![chunks[2].clone()]).unwrap(),
            ),
        ],
        None,
        &token(),
    )
    .unwrap();
    let result = service(&s)
        .search_papers("tail".into(), 5, token())
        .await
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].paper_id, "p");
    assert_eq!(result[0].page, 2);
    assert_eq!(result[0].snippet, format!("{}…", "é".repeat(240)));
}
#[tokio::test]
async fn restart_persisted_lexical_roundtrip_same_count_replacement_and_corrupt_cache_fallback() {
    // test_save_and_load_round_trip_preserves_scores and stale-cache regression.
    let (temp, s) = store();
    replace(
        &s,
        vec![
            record("a", 0, "transformer attention model", 0.1),
            record("b", 1, "graph neural network survey", 0.2),
            record("c", 2, "attention attention transformer study", 0.3),
            record("d", 3, "clustering methods overview", 0.4),
        ],
    );
    s.apply(&[Op::RebuildLexical], None, &token()).unwrap();
    assert_eq!(s.status(&token()).unwrap().lexical, LexicalStatus::Ready);
    let reopened = VectorStore::new(temp.path(), s.database_path(), identity()).unwrap();
    let before = service(&reopened)
        .retrieve_typed(
            request("attention", Strategy::Bm25, Scope::FullCorpus, 5),
            token(),
        )
        .await
        .unwrap();
    let before_revision = s.status(&token()).unwrap().revision;
    replace(
        &s,
        vec![record("c", 2, "unrelated unrelated unrelated study", 0.3)],
    );
    let status = reopened.status(&token()).unwrap();
    assert_eq!(status.chunk_count, 4);
    assert!(status.revision > before_revision);
    assert_eq!(status.lexical, LexicalStatus::Stale);
    let after = service(&reopened)
        .retrieve_typed(
            request("attention", Strategy::Bm25, Scope::FullCorpus, 5),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(ids(&before)[0], "c");
    assert_eq!(ids(&after)[0], "a");
    s.apply(&[Op::RebuildLexical], None, &token()).unwrap();
    let db = sqlite::open(s.database_path()).unwrap();
    db.execute("UPDATE lexical SET payload='{broken'").unwrap();
    drop(db);
    assert_eq!(
        reopened.status(&token()).unwrap().lexical,
        LexicalStatus::Stale
    );
    let fallback = service(&reopened)
        .retrieve_typed(
            request("attention", Strategy::Bm25, Scope::FullCorpus, 5),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(fallback).unwrap()
    );
}
#[test]
fn replacement_is_idempotent_removes_obsolete_chunks_and_rolls_back_whole_batch() {
    let (_temp, s) = store();
    let paper = PreparedPaper::new(
        "p".into(),
        identity(),
        vec![record("p", 0, "old", 0.1), record("p", 1, "retained", 0.2)],
    )
    .unwrap();
    s.apply(&[Op::ReplacePaper(paper.clone())], None, &token())
        .unwrap();
    let before = s.status(&token()).unwrap();
    assert_eq!(
        s.apply(&[Op::ReplacePaper(paper)], None, &token()).unwrap(),
        before
    );
    let replacement =
        PreparedPaper::new("p".into(), identity(), vec![record("p", 1, "new", 0.2)]).unwrap();
    s.apply(
        &[Op::ReplacePaper(replacement.clone())],
        Some(before.revision),
        &token(),
    )
    .unwrap();
    assert_eq!(s.read_paper("p", &token()).unwrap().len(), 1);
    assert_eq!(
        s.apply(
            &[Op::DeletePaper("p".into())],
            Some(before.revision),
            &token()
        ),
        Err(E::Conflict)
    );
    let mut stolen = record("other", 0, "bad", 0.0);
    stolen.id = "p:1".into();
    let bad = PreparedPaper::new("other".into(), identity(), vec![stolen]).unwrap();
    assert_eq!(
        s.apply(
            &[
                Op::ReplacePaper(
                    PreparedPaper::new("new".into(), identity(), vec![record("new", 0, "ok", 0.0)])
                        .unwrap()
                ),
                Op::ReplacePaper(bad)
            ],
            None,
            &token()
        ),
        Err(E::Conflict)
    );
    assert_eq!(s.status(&token()).unwrap().chunk_count, 1);
    let c = token();
    s.apply(&[Op::ReplacePaper(replacement)], None, &c).unwrap();
    assert!(!c.cancel());
    assert_eq!(
        s.apply(&[Op::DeletePaper("p".into())], None, &c),
        Err(E::Conflict)
    );
    assert_eq!(s.status(&token()).unwrap().chunk_count, 1);
    s.apply(&[Op::DeletePaper("p".into())], None, &token())
        .unwrap();
    assert_eq!(s.status(&token()).unwrap().chunk_count, 0);
}
#[test]
fn model_and_dimension_changes_require_explicit_reset_and_old_handles_reject_new_identity() {
    let (temp, s) = store();
    replace(&s, vec![record("p", 0, "text", 0.0)]);
    for id in [
        EmbeddingIdentity {
            model: "different".into(),
            dimension: 1,
        },
        EmbeddingIdentity {
            model: identity().model,
            dimension: 2,
        },
    ] {
        let next = VectorStore::new(temp.path(), s.database_path(), id.clone()).unwrap();
        assert_eq!(next.initialize(&token()), Err(E::ReindexRequired));
        assert_eq!(s.status(&token()).unwrap().chunk_count, 1);
    }
    let next_id = EmbeddingIdentity {
        model: "new".into(),
        dimension: 2,
    };
    let next = VectorStore::new(temp.path(), s.database_path(), next_id.clone()).unwrap();
    let c = token();
    c.cancel();
    assert_eq!(
        next.apply(
            &[Op::Reset(PreparedReset::new(next_id.clone()).unwrap())],
            None,
            &c
        ),
        Err(E::Cancelled)
    );
    next.apply(
        &[
            Op::Reset(PreparedReset::new(next_id).unwrap()),
            Op::RebuildLexical,
        ],
        None,
        &token(),
    )
    .unwrap();
    assert_eq!(next.status(&token()).unwrap().chunk_count, 0);
    assert_eq!(s.status(&token()), Err(E::ReindexRequired));
}
#[test]
fn invalid_vectors_provenance_dtos_and_legacy_stores_are_rejected_without_reset() {
    for vector in [vec![], vec![0.0, 1.0], vec![f64::NAN], vec![f64::INFINITY]] {
        let mut r = record("p", 0, "text", 0.0);
        r.vector = vector;
        assert!(PreparedPaper::new("p".into(), identity(), vec![r]).is_err());
    }
    let mut r = record("p", 0, "text", 0.0);
    r.page = 0;
    assert!(PreparedPaper::new("p".into(), identity(), vec![r]).is_err());
    assert!(
        PreparedPaper::new("p".into(), identity(), vec![record("p", 0, "text", 0.0); 2]).is_err()
    );
    assert!(
        serde_json::from_value::<SearchRequest>(
            serde_json::json!({"query":"q","strategy":"vector","scope":"full_corpus","limit":true})
        )
        .is_err()
    );
    assert!(
        request(" ", Strategy::Hybrid, Scope::ChatCandidates, 5)
            .validate()
            .is_err()
    );
    assert!(
        request("q", Strategy::Vector, Scope::ChatCandidates, 5)
            .validate()
            .is_err()
    );
    assert!(
        request("q", Strategy::Vector, Scope::FullCorpus, 0)
            .validate()
            .is_err()
    );
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("legacy.sqlite");
    let db = sqlite::open(&path).unwrap();
    db.execute("CREATE TABLE collections(id TEXT); INSERT INTO collections VALUES('legacy')")
        .unwrap();
    drop(db);
    let bytes = std::fs::read(&path).unwrap();
    let s = VectorStore::new(temp.path(), &path, identity()).unwrap();
    assert_eq!(s.initialize(&token()), Err(E::InvalidStore));
    assert!(
        s.apply(
            &[Op::Reset(PreparedReset::new(identity()).unwrap())],
            None,
            &token()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
#[tokio::test]
async fn empty_corpus_cancelled_requests_and_invalid_provider_never_publish() {
    let (_temp, s) = store();
    let fake = Arc::new(Fake {
        fail: true,
        ..Fake::default()
    });
    let svc = NativeRetriever::new(s.clone(), fake.clone(), None);
    assert!(
        svc.retrieve_typed(
            request("q", Strategy::Hybrid, Scope::FullCorpus, 5),
            token()
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    replace(&s, vec![record("p", 0, "text", 0.0)]);
    assert_eq!(
        svc.retrieve_typed(
            request("q", Strategy::Vector, Scope::FullCorpus, 5),
            token()
        )
        .await
        .unwrap_err(),
        E::Provider
    );
    let c = token();
    c.cancel();
    assert_eq!(
        service(&s)
            .retrieve_typed(request("q", Strategy::Vector, Scope::FullCorpus, 5), c)
            .await
            .unwrap_err(),
        E::Cancelled
    );
    let invalid = NativeRetriever::new(
        s.clone(),
        Arc::new(Fake {
            vector: vec![f64::NAN],
            ..Fake::default()
        }),
        None,
    );
    assert_eq!(
        invalid
            .retrieve_typed(
                request("q", Strategy::Vector, Scope::FullCorpus, 5),
                token()
            )
            .await
            .unwrap_err(),
        E::InvalidInput
    );
}
#[tokio::test]
async fn canonical_history_labeling_idempotency_session_filter_hydration_and_limits() {
    let (temp, s) = store();
    let history = SqliteHistory::new(temp.path(), &temp.path().join("history.sqlite")).unwrap();
    history.initialize(&token()).unwrap();
    let first = history
        .append_exchange_sync("alpha", "question", "answer", &[], &token())
        .unwrap();
    let second = history
        .append_exchange_sync("beta", "other", "canonical", &[], &token())
        .unwrap();
    let fake = Fake::zero();
    let svc = NativeRetriever::new(s.clone(), fake.clone(), Some(history.clone()));
    svc.index_exchange(first, token()).await.unwrap();
    svc.index_exchange(first, token()).await.unwrap();
    svc.index_exchange(second, token()).await.unwrap();
    assert_eq!(s.status(&token()).unwrap().exchange_count, 2);
    assert_eq!(
        fake.calls.lock().unwrap()[0],
        vec!["User: question\nAssistant: answer"]
    );
    let all = svc
        .search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: None,
                limit: 2,
            },
            token(),
        )
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|e| e.exchange_id).collect::<Vec<_>>(),
        [first, second]
    );
    let filtered = svc
        .search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: Some("beta".into()),
                limit: 1,
            },
            token(),
        )
        .await
        .unwrap();
    assert_eq!(filtered[0].exchange_id, second);
    // Change canonical content after indexing; vector context must never hydrate it.
    let db = sqlite::open(history.database_path()).unwrap();
    db.execute("UPDATE messages SET content='fresh canonical' WHERE session_id='beta' AND role='assistant'").unwrap();
    let filtered = svc
        .search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: Some("beta".into()),
                limit: 1,
            },
            token(),
        )
        .await
        .unwrap();
    assert_eq!(filtered[0].assistant_message.content, "fresh canonical");
    db.execute("DELETE FROM messages WHERE session_id='beta'")
        .unwrap();
    assert_eq!(
        svc.search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: Some("beta".into()),
                limit: 1
            },
            token()
        )
        .await
        .unwrap_err(),
        E::History
    );
    assert!(
        svc.search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: Some(" ".into()),
                limit: 1
            },
            token()
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn prepare_paper_does_not_write_and_uses_one_linked_final_token() {
    let (_temp, s) = store();
    let svc = service(&s);
    let c = token();
    let chunk = Chunk {
        text: "new paper".into(),
        source: SourceRecord {
            rank: 0,
            paper_id: "p".into(),
            chunk_index: 0,
            filename: "p.pdf".into(),
            title: "P".into(),
            page: 1,
            score: 0.0,
            snippet: String::new(),
        },
    };
    let prepared = svc
        .prepare_paper("p".into(), vec![chunk], c.clone())
        .await
        .unwrap();
    assert_eq!(s.status(&token()).unwrap().chunk_count, 0);
    assert_eq!(prepared.chunks()[0].vector, [0.0]);
    svc.apply(
        vec![Op::ReplacePaper(prepared), Op::RebuildLexical],
        Some(0),
        c.clone(),
    )
    .await
    .unwrap();
    assert!(!c.cancel());
    assert_eq!(s.status(&token()).unwrap().chunk_count, 1);
}

#[tokio::test]
async fn exact_distance_ties_survive_replacement_and_reopen_but_lexical_uses_chunk_key() {
    let (temp, s) = store();
    let records = vec![record("p", 3, "same", 1.0), record("p", 0, "same", 1.0)];
    let prepared = PreparedPaper::new("p".into(), identity(), records).unwrap();
    s.apply(&[Op::ReplacePaper(prepared.clone())], None, &token())
        .unwrap();
    s.apply(&[Op::ReplacePaper(prepared)], None, &token())
        .unwrap();
    let s = VectorStore::new(temp.path(), s.database_path(), identity()).unwrap();
    let svc = service(&s);
    for strategy in [Strategy::Vector, Strategy::Hybrid, Strategy::Bm25] {
        let result = svc
            .retrieve_typed(request("same", strategy, Scope::FullCorpus, 5), token())
            .await
            .unwrap();
        let expected = if strategy == Strategy::Vector {
            vec![3, 0]
        } else {
            vec![0, 3]
        };
        assert_eq!(
            result
                .iter()
                .map(|r| r.source.chunk_index)
                .collect::<Vec<_>>(),
            expected
        );
    }
    let chat = svc
        .retrieve_typed(
            request("same", Strategy::Hybrid, Scope::ChatCandidates, 5),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(chat[0].source.chunk_index, 3);
}

#[tokio::test]
async fn candidate_local_statistics_exclude_the_sixth_vector_for_limit_one() {
    let (_temp, s) = store();
    replace(
        &s,
        (0..6)
            .map(|i| {
                record(
                    &format!("p{i}"),
                    0,
                    if i == 5 { "needle" } else { "other" },
                    i as f64,
                )
            })
            .collect(),
    );
    let svc = service(&s);
    let chat = svc
        .retrieve_typed(
            request("needle", Strategy::Hybrid, Scope::ChatCandidates, 1),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(chat[0].source.paper_id, "p0");
    // Candidate-local lexical absence gives exactly .5, not a full-corpus IDF blend.
    assert_eq!(chat[0].source.score, 0.5);
    let full = svc
        .retrieve_typed(
            request("needle", Strategy::Bm25, Scope::FullCorpus, 1),
            token(),
        )
        .await
        .unwrap();
    assert_eq!(full[0].source.paper_id, "p5");
    assert_eq!(
        svc.retrieve_typed(
            request(
                "needle",
                Strategy::Hybrid,
                Scope::ChatCandidates,
                usize::MAX
            ),
            token()
        )
        .await
        .unwrap()
        .len(),
        6
    );
}

#[tokio::test]
async fn persisted_bm25_matches_source_corpus_formula_for_every_reference_query() {
    let (_temp, s) = store();
    let texts = [
        "transformer attention model",
        "graph neural network survey",
        "attention attention transformer study",
        "clustering methods overview",
    ];
    replace(
        &s,
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| record(&format!("p{i}"), 0, t, 0.0))
            .collect(),
    );
    s.apply(&[Op::RebuildLexical], None, &token()).unwrap();
    for q in [
        "transformer attention",
        "graph",
        "missing",
        "network survey",
    ] {
        let direct = bm25_scores(q, &texts, &token()).unwrap();
        let results = service(&s)
            .retrieve_typed(request(q, Strategy::Bm25, Scope::FullCorpus, 5), token())
            .await
            .unwrap();
        for r in results {
            let i = r
                .source
                .paper_id
                .trim_start_matches('p')
                .parse::<usize>()
                .unwrap();
            assert!((r.source.score - direct[i]).abs() < 1e-12);
        }
    }
}

#[tokio::test]
async fn explicit_rebuild_replaces_model_and_vectors_together_and_never_canonical_history() {
    let (temp, s) = store();
    let history = SqliteHistory::new(temp.path(), &temp.path().join("history.sqlite")).unwrap();
    history.initialize(&token()).unwrap();
    let id = history
        .append_exchange_sync("session", "user", "answer", &[], &token())
        .unwrap();
    NativeRetriever::new(s.clone(), Fake::zero(), Some(history.clone()))
        .index_exchange(id, token())
        .await
        .unwrap();
    replace(&s, vec![record("old", 0, "old text", 0.0)]);
    let next_id = EmbeddingIdentity {
        model: "next".into(),
        dimension: 2,
    };
    let next = VectorStore::new(temp.path(), s.database_path(), next_id.clone()).unwrap();
    let mut r = record("new", 0, "new text", 0.0);
    r.vector = vec![0.0, 1.0];
    let status = next
        .apply(
            &[
                Op::Reset(PreparedReset::new(next_id.clone()).unwrap()),
                Op::ReplacePaper(PreparedPaper::new("new".into(), next_id, vec![r]).unwrap()),
                Op::RebuildLexical,
            ],
            None,
            &token(),
        )
        .unwrap();
    assert_eq!(
        (status.chunk_count, status.exchange_count, status.lexical),
        (1, 0, LexicalStatus::Ready)
    );
    assert_eq!(
        next.read_paper("new", &token()).unwrap()[0].vector,
        [0.0, 1.0]
    );
    assert_eq!(
        history
            .get_exchange(id, "session")
            .unwrap()
            .assistant_message
            .content,
        "answer"
    );
}

#[test]
fn app_owned_path_validation_rejects_outside_and_alias_files() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("app");
    std::fs::create_dir(&root).unwrap();
    assert!(VectorStore::new(&root, &temp.path().join("outside.sqlite"), identity()).is_err());
    let target = root.join("original.sqlite");
    std::fs::write(&target, b"not a store").unwrap();
    let alias = root.join("alias.sqlite");
    std::fs::hard_link(&target, &alias).unwrap();
    assert!(VectorStore::new(&root, &alias, identity()).is_err());
    #[cfg(unix)]
    {
        let link = root.join("link.sqlite");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(VectorStore::new(&root, &link, identity()).is_err());
    }
}

#[test]
fn validation_matches_python_strip_for_information_separators_and_strict_wire_fields() {
    for q in ["", "\u{1c}", "\u{1d}\u{1e}\u{1f}", "\u{85}\u{a0}"] {
        assert_eq!(
            request(q, Strategy::Hybrid, Scope::ChatCandidates, 1).validate(),
            Err(E::InvalidInput)
        );
        assert!(
            HistorySearchRequest {
                query: "q".into(),
                session_id: Some(q.into()),
                limit: 1
            }
            .validate()
            .is_err()
        );
    }
    let r = serde_json::json!({"query":"q","strategy":"hybrid","scope":"chat_candidates","limit":1,"unknown":0});
    assert!(serde_json::from_value::<SearchRequest>(r).is_err());
    assert!(serde_json::from_value::<EmbeddedChunk>(serde_json::json!({"id":"i","paper_id":"p","chunk_index":0,"filename":"p.pdf","title":"P","page":1,"text":"t","vector":[true]})).is_err());
}

#[tokio::test]
async fn history_search_uses_unrounded_squared_distance_order_and_preserves_sources_after_reopen() {
    struct Ordered;
    impl EmbeddingClient for Ordered {
        fn embed(&self, texts: Vec<String>, c: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
            Box::pin(async move {
                c.check()?;
                Ok(texts
                    .into_iter()
                    .map(|s| {
                        vec![if s.starts_with("User: far") {
                            3.0
                        } else if s.starts_with("User:") {
                            1.0
                        } else {
                            0.0
                        }]
                    })
                    .collect())
            })
        }
    }
    let (temp, s) = store();
    let path = temp.path().join("history.sqlite");
    let history = SqliteHistory::new(temp.path(), &path).unwrap();
    history.initialize(&token()).unwrap();
    let source = SourceRecord {
        rank: 1,
        paper_id: "p".into(),
        chunk_index: 0,
        filename: "p.pdf".into(),
        title: "P".into(),
        page: 1,
        score: 0.75,
        snippet: "evidence".into(),
    };
    let far = history
        .append_exchange_sync("alpha", "far", "answer", &[], &token())
        .unwrap();
    let near = history
        .append_exchange_sync(
            "beta",
            "near",
            "answer",
            std::slice::from_ref(&source),
            &token(),
        )
        .unwrap();
    let svc = NativeRetriever::new(s.clone(), Arc::new(Ordered), Some(history));
    svc.index_exchange(far, token()).await.unwrap();
    svc.index_exchange(near, token()).await.unwrap();
    let svc = NativeRetriever::new(
        VectorStore::new(temp.path(), s.database_path(), identity()).unwrap(),
        Arc::new(Ordered),
        Some(SqliteHistory::new(temp.path(), &path).unwrap()),
    );
    let result = svc
        .search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: None,
                limit: 2,
            },
            token(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.iter().map(|e| e.exchange_id).collect::<Vec<_>>(),
        [near, far]
    );
    assert_eq!(result[0].assistant_message.sources, vec![source]);
    let filtered = svc
        .search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: Some("alpha".into()),
                limit: 1,
            },
            token(),
        )
        .await
        .unwrap();
    assert_eq!(filtered[0].exchange_id, far);
    s.apply(&[Op::DeleteExchange(near)], None, &token())
        .unwrap();
    assert_eq!(
        svc.search_history_typed(
            HistorySearchRequest {
                query: "q".into(),
                session_id: None,
                limit: 1
            },
            token()
        )
        .await
        .unwrap()[0]
            .exchange_id,
        far
    );
}

#[tokio::test]
async fn cancellation_during_embedding_does_not_publish_prepared_data() {
    struct Cancel;
    impl EmbeddingClient for Cancel {
        fn embed(&self, texts: Vec<String>, c: Cancellation) -> BackendFuture<Vec<Vec<f64>>> {
            Box::pin(async move {
                assert!(c.cancel());
                Ok(vec![vec![0.0]; texts.len()])
            })
        }
    }
    let (_temp, s) = store();
    let svc = NativeRetriever::new(s.clone(), Arc::new(Cancel), None);
    let c = token();
    let chunk = Chunk {
        text: "text".into(),
        source: SourceRecord {
            rank: 0,
            paper_id: "p".into(),
            chunk_index: 0,
            filename: "p.pdf".into(),
            title: "P".into(),
            page: 1,
            score: 0.0,
            snippet: String::new(),
        },
    };
    assert_eq!(
        svc.prepare_paper("p".into(), vec![chunk], c.clone())
            .await
            .unwrap_err(),
        E::Cancelled
    );
    assert_eq!(s.status(&token()).unwrap().chunk_count, 0);
    assert_eq!(c.begin_commit().unwrap_err(), ErrorCode::Cancelled);
}
