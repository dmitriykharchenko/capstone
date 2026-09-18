use super::{
    EmbeddedChunk, IndexSnapshot, IndexStatus, PaperMatch, PreparedExchange, PreparedOperation,
    PreparedPaper, RetrievalError, RetrievalResult, Retriever, SearchRequest, VectorStore, ranking,
    types::{ExchangeVector, check, nonempty},
};
use crate::{
    domain::{Chunk, RetrievalScope, RetrievalStrategy},
    history::{ExchangeRecord, HistoryError, SqliteHistory},
    protocol::ErrorCode,
    providers::EmbeddingClient,
    registry::BackendFuture,
    runtime::Cancellation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySearchRequest {
    pub query: String,
    pub session_id: Option<String>,
    pub limit: usize,
}
impl HistorySearchRequest {
    pub fn validate(&self) -> RetrievalResult<()> {
        nonempty(&self.query)?;
        if self.limit == 0 {
            return Err(RetrievalError::InvalidInput);
        }
        if let Some(s) = &self.session_id {
            nonempty(s)?;
        }
        Ok(())
    }
}

/// The composition root must supply a provider matching store.identity(). The
/// existing EmbeddingClient trait does not expose model identity; dimensions and
/// all values are independently validated here. No implicit model migration.
#[derive(Clone)]
pub struct NativeRetriever {
    store: VectorStore,
    embeddings: Arc<dyn EmbeddingClient>,
    history: Option<SqliteHistory>,
}
impl NativeRetriever {
    pub fn new(
        store: VectorStore,
        embeddings: Arc<dyn EmbeddingClient>,
        history: Option<SqliteHistory>,
    ) -> Self {
        Self {
            store,
            embeddings,
            history,
        }
    }
    pub async fn initialize(&self, c: Cancellation) -> RetrievalResult<()> {
        let store = self.store.clone();
        blocking(move || store.initialize(&c)).await
    }
    pub async fn snapshot(&self, c: Cancellation) -> RetrievalResult<IndexSnapshot> {
        let store = self.store.clone();
        blocking(move || store.snapshot(&c)).await
    }
    pub async fn status(&self, c: Cancellation) -> RetrievalResult<IndexStatus> {
        Ok(self.snapshot(c).await?.status)
    }
    pub async fn apply(
        &self,
        operations: Vec<PreparedOperation>,
        expected_revision: Option<i64>,
        c: Cancellation,
    ) -> RetrievalResult<IndexStatus> {
        let store = self.store.clone();
        blocking(move || store.apply(&operations, expected_revision, &c)).await
    }
    /// Prepare from already-extracted chunks. Empty means explicit paper removal.
    /// Preparation never seals the request token, so the same linked token may be
    /// used through embedding/preparation and ONE later batch apply.
    pub async fn prepare_paper(
        &self,
        paper_id: String,
        chunks: Vec<Chunk>,
        c: Cancellation,
    ) -> RetrievalResult<PreparedPaper> {
        check(&c)?;
        nonempty(&paper_id)?;
        let identity = self.store.identity().clone();
        let mut records: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let id = format!("{}:{}", chunk.source.paper_id, chunk.source.chunk_index);
                EmbeddedChunk::from_chunk(id, chunk, vec![0.0; identity.dimension])
            })
            .collect();
        // Validate provenance/duplicates before spending any embedding request.
        PreparedPaper::new(paper_id.clone(), identity.clone(), records.clone())?;
        if !records.is_empty() {
            let vectors = self
                .embed(records.iter().map(|r| r.text.clone()).collect(), c.clone())
                .await?;
            for (record, vector) in records.iter_mut().zip(vectors) {
                record.vector = vector;
            }
        }
        check(&c)?;
        PreparedPaper::new(paper_id, identity, records)
    }
    async fn embed(&self, texts: Vec<String>, c: Cancellation) -> RetrievalResult<Vec<Vec<f64>>> {
        check(&c)?;
        let count = texts.len();
        let vectors = self.embeddings.embed(texts, c.clone()).await.map_err(|e| {
            if e == ErrorCode::Cancelled {
                RetrievalError::Cancelled
            } else {
                RetrievalError::Provider
            }
        })?;
        check(&c)?;
        if vectors.len() != count {
            return Err(RetrievalError::Provider);
        }
        for vector in &vectors {
            self.store.identity().vector(vector)?;
        }
        Ok(vectors)
    }
    pub async fn retrieve_typed(
        &self,
        request: SearchRequest,
        c: Cancellation,
    ) -> RetrievalResult<Vec<Chunk>> {
        request.validate()?;
        check(&c)?;
        let snapshot = self.snapshot(c.clone()).await?;
        if snapshot.chunks.is_empty() {
            return Ok(vec![]);
        }
        let vector = if request.strategy == RetrievalStrategy::Bm25 {
            None
        } else {
            Some(
                self.embed(vec![request.query.clone()], c.clone())
                    .await?
                    .remove(0),
            )
        };
        blocking(move || ranking::retrieve(&snapshot, &request, vector.as_deref(), &c)).await
    }
    pub async fn search_papers(
        &self,
        query: String,
        limit: usize,
        c: Cancellation,
    ) -> RetrievalResult<Vec<PaperMatch>> {
        SearchRequest {
            query: query.clone(),
            limit,
            strategy: RetrievalStrategy::Hybrid,
            scope: RetrievalScope::ChatCandidates,
        }
        .validate()?;
        let snapshot = self.snapshot(c.clone()).await?;
        if snapshot.chunks.is_empty() {
            return Ok(vec![]);
        }
        let vector = self.embed(vec![query.clone()], c.clone()).await?.remove(0);
        blocking(move || {
            let scores = ranking::candidate_scores(&snapshot.chunks, &query, &vector, limit, &c)?;
            check(&c)?;
            Ok(ranking::group(&snapshot.chunks, scores, limit))
        })
        .await
    }
    /// Canonical SQLite hydration, never accepting a caller-supplied exchange body.
    /// Supply session_id when known to avoid the ID-only trait's session scan.
    pub async fn prepare_exchange(
        &self,
        exchange_id: i64,
        session_id: Option<String>,
        c: Cancellation,
    ) -> RetrievalResult<PreparedExchange> {
        if exchange_id <= 0 {
            return Err(RetrievalError::InvalidInput);
        }
        if let Some(s) = &session_id {
            nonempty(s)?;
        }
        let history = self.history.clone().ok_or(RetrievalError::History)?;
        let read_c = c.clone();
        let exchange = blocking(move || {
            check(&read_c)?;
            if let Some(session) = session_id {
                return history
                    .get_exchange(exchange_id, &session)
                    .map_err(|_| RetrievalError::History);
            }
            // Frozen HistoryStore has no ID-only lookup. Use public canonical
            // methods rather than bypassing its schema/path/source validation.
            for session in history
                .list_sessions()
                .map_err(|_| RetrievalError::History)?
            {
                check(&read_c)?;
                match history.get_exchange(exchange_id, &session.session_id) {
                    Ok(exchange) => return Ok(exchange),
                    Err(HistoryError::NotFound) => (),
                    Err(_) => return Err(RetrievalError::History),
                }
            }
            Err(RetrievalError::History)
        })
        .await?;
        let context = format!(
            "User: {}\nAssistant: {}",
            exchange.user_message.content, exchange.assistant_message.content
        );
        let vector = self
            .embed(vec![context.clone()], c.clone())
            .await?
            .remove(0);
        let record = ExchangeVector {
            exchange_id,
            session_id: exchange.session_id,
            context,
            vector,
        };
        record.validate(self.store.identity())?;
        Ok(PreparedExchange {
            identity: self.store.identity().clone(),
            record,
        })
    }
    pub async fn search_history_typed(
        &self,
        request: HistorySearchRequest,
        c: Cancellation,
    ) -> RetrievalResult<Vec<ExchangeRecord>> {
        request.validate()?;
        check(&c)?;
        let history = self.history.clone().ok_or(RetrievalError::History)?;
        let snapshot = self.snapshot(c.clone()).await?;
        if snapshot.exchanges.is_empty() {
            return Ok(vec![]);
        }
        let vector = self.embed(vec![request.query], c.clone()).await?.remove(0);
        blocking(move || {
            let mut ranked = vec![];
            for record in snapshot.exchanges {
                check(&c)?;
                if request
                    .session_id
                    .as_ref()
                    .is_some_and(|s| s != &record.session_id)
                {
                    continue;
                }
                ranked.push((ranking::squared_l2(&vector, &record.vector)?, record));
            }
            ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut results = vec![];
            for (_, record) in ranked.into_iter().take(request.limit) {
                check(&c)?;
                results.push(
                    history
                        .get_exchange(record.exchange_id, &record.session_id)
                        .map_err(|_| RetrievalError::History)?,
                );
            }
            check(&c)?;
            Ok(results)
        })
        .await
    }
}
impl Retriever for NativeRetriever {
    fn retrieve(
        &self,
        query: String,
        strategy: RetrievalStrategy,
        scope: RetrievalScope,
        limit: usize,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Chunk>> {
        let service = self.clone();
        Box::pin(async move {
            service
                .retrieve_typed(
                    SearchRequest {
                        query,
                        strategy,
                        scope,
                        limit,
                    },
                    cancellation,
                )
                .await
                .map_err(Into::into)
        })
    }
    fn search_history(
        &self,
        query: String,
        session_id: Option<String>,
        limit: usize,
        cancellation: Cancellation,
    ) -> BackendFuture<Value> {
        let service = self.clone();
        Box::pin(async move {
            let matches = service
                .search_history_typed(
                    HistorySearchRequest {
                        query,
                        session_id,
                        limit,
                    },
                    cancellation,
                )
                .await?;
            serde_json::to_value(matches).map_err(|_| ErrorCode::Internal)
        })
    }
    fn index_exchange(&self, exchange_id: i64, cancellation: Cancellation) -> BackendFuture<()> {
        let service = self.clone();
        Box::pin(async move {
            let prepared = service
                .prepare_exchange(exchange_id, None, cancellation.clone())
                .await?;
            service
                .apply(
                    vec![PreparedOperation::UpsertExchange(prepared)],
                    None,
                    cancellation,
                )
                .await?;
            Ok(())
        })
    }
}
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> RetrievalResult<T> + Send + 'static,
) -> RetrievalResult<T> {
    // Never select/drop this join on cancellation: native transactions must finish
    // rollback/commit and report their actual outcome before terminal publication.
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| RetrievalError::Storage)?
}
