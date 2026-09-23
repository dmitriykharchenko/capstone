use super::*;
use crate::{
    history::{CommittedTurn, SqliteHistory},
    providers::EmbeddingClient,
    retrieval::{NativeRetriever, PreparedExchangeArtifact},
};
use futures_util::FutureExt;
use serde::Serialize;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize)]
pub struct HistoryIndexObservation {
    pub reset_epoch: u64,
    pub retrieval_revision: i64,
    pub indexed: bool,
}
impl CoordinatedIndex {
    /// Assemble before recovery; history reads remain separate transactions.
    pub fn with_history(mut self, history: SqliteHistory) -> Self {
        self.history = Some(history);
        self
    }
    pub fn retriever(&self, embeddings: Arc<dyn EmbeddingClient>) -> NativeRetriever {
        NativeRetriever::new(self.retrieval.clone(), embeddings, self.history.clone())
    }
    async fn canonical(&self, id: i64) -> IndexResult<CommittedTurn> {
        let h = self.history.clone().ok_or(IndexError::InvalidJournal)?;
        joined(Cancellation::default(), move |_| {
            h.verified_intent(id)
                .map_err(|_| IndexError::InvalidJournal)
        })
        .await
    }
    pub(super) async fn recover_exchange_intents(&self) -> IndexResult<()> {
        let Some(history) = self.history.clone() else {
            return Ok(());
        };
        let mut cursor = self.journal(Lane::Startup, |j, _| j.scan_cursor()).await?;
        loop {
            let h = history.clone();
            let batch = joined(Cancellation::default(), move |_| {
                h.list_exchange_index_intents(cursor, 256)
                    .map_err(|_| IndexError::InvalidJournal)
            })
            .await?;
            let Some(last) = batch.last() else { break };
            cursor = last.exchange_id;
            self.journal(Lane::Startup, move |j, c| {
                j.seed_intents(&batch, Some(cursor), &c)
            })
            .await?;
        }
        Ok(())
    }
    pub(super) async fn verify_exchanges(&self, meta: &Meta, count: usize) -> IndexResult<()> {
        let mut cursor = 0;
        let mut indexed = 0;
        loop {
            let Some(entry) = self
                .journal(Lane::Startup, move |j, _| j.next_exchange(cursor))
                .await?
            else {
                break;
            };
            cursor = entry.intent.exchange_id;
            if entry.epoch != meta.epoch || self.canonical(cursor).await?.intent != entry.intent {
                return Err(IndexError::InvalidJournal);
            }
            if entry.complete {
                let artifact = entry.artifact.ok_or(IndexError::InvalidJournal)?;
                artifact.verify(&self.canonical(cursor).await?)?;
                let store = self.retrieval.clone();
                if !joined(Cancellation::default(), move |c| {
                    Ok(store.exchange_matches(&artifact, &c)?)
                })
                .await?
                {
                    return Err(IndexError::InvalidJournal);
                }
                indexed += 1;
            } else if entry.artifact.is_some() {
                return Err(IndexError::InvalidJournal);
            }
        }
        if indexed != count {
            return Err(IndexError::InvalidJournal);
        }
        Ok(())
    }
    pub async fn observe_exchange(&self, id: i64) -> IndexResult<HistoryIndexObservation> {
        let _guard = self.gate.read().await?;
        let result = async {
            self.verify_ready().await?;
            let meta = self
                .journal(Lane::Startup, |j, _| j.meta())
                .await?
                .ok_or(IndexError::InvalidJournal)?;
            let entry = self
                .journal(Lane::Startup, move |j, _| j.exchange_entry(id))
                .await?;
            Ok(HistoryIndexObservation {
                reset_epoch: meta.epoch,
                retrieval_revision: meta.revision,
                indexed: entry.is_some_and(|e| e.complete && e.epoch == meta.epoch),
            })
        }
        .await;
        if result.is_err() {
            self.gate.poison();
        }
        result
    }
    /// One original-turn attempt only. Embedding runs outside the write gate; a
    /// reset in that interval invalidates the captured epoch and forbids apply.
    pub async fn index_chat_exchange(
        &self,
        canonical: CommittedTurn,
        retriever: NativeRetriever,
        group: &OperationGroup,
    ) -> IndexResult<()> {
        let epoch = {
            let mut guard = self.gate.write(false).await?;
            guard.arm();
            self.verify_ready().await?;
            let meta = self
                .journal(Lane::Startup, |j, _| j.meta())
                .await?
                .ok_or(IndexError::InvalidJournal)?;
            let intent = canonical.intent.clone();
            self.journal(Lane::Derived(group), move |j, c| {
                j.seed_intents(&[intent], None, &c)
            })
            .await?;
            guard.ready();
            meta.epoch
        };
        let id = canonical.intent.exchange_id;
        let session = canonical.intent.session_id.clone();
        let prepared = group
            .run_derived(move |c| async move {
                AssertUnwindSafe(retriever.prepare_exchange(id, Some(session), c))
                    .catch_unwind()
                    .await
                    .map_err(|_| crate::protocol::ErrorCode::Internal)?
                    .map_err(Into::into)
            })
            .await
            .map_err(IndexError::from)?
            .map_err(IndexError::from)?;
        let artifact = PreparedExchangeArtifact::new(prepared, &canonical)?;
        let mut guard = self.gate.write(false).await?;
        guard.arm();
        self.verify_ready().await?;
        let meta = self
            .journal(Lane::Startup, |j, _| j.meta())
            .await?
            .ok_or(IndexError::InvalidJournal)?;
        if meta.epoch != epoch {
            guard.ready();
            return Err(IndexError::Conflict);
        }
        artifact.verify(&self.canonical(id).await?)?;
        let entry = journal::ExchangeEntry {
            intent: canonical.intent,
            epoch,
            artifact: Some(artifact),
            complete: false,
        };
        let op = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            base: meta,
            phase: Phase::Prepared,
            kind: Kind::Exchange(Box::new(entry)),
        };
        let save = op.clone();
        self.journal(Lane::Derived(group), move |j, c| j.prepare(&save, &c))
            .await?;
        self.resolve_exchange(op, Lane::Derived(group)).await?;
        self.verify_ready().await?;
        guard.ready();
        Ok(())
    }
    pub(super) async fn resolve_exchange(&self, op: Operation, lane: Lane<'_>) -> IndexResult<()> {
        let Kind::Exchange(entry) = &op.kind else {
            return Err(IndexError::InvalidJournal);
        };
        let artifact = entry.artifact.clone().ok_or(IndexError::InvalidJournal)?;
        artifact.verify(&self.canonical(entry.intent.exchange_id).await?)?;
        let check = op.clone();
        self.journal(lane, move |j, _| j.validate_pending(&check))
            .await?;
        if self.fence().await? != op.base.fence {
            return Err(IndexError::InvalidJournal);
        }
        let status = self.status().await?;
        if status.revision == op.base.revision {
            let store = self.retrieval.clone();
            let prepared = artifact.restore(&op.base.identity)?;
            lane.run(move |c| {
                joined(c, move |c| {
                    Ok(store.apply(
                        &[
                            PreparedOperation::UpsertExchange(prepared),
                            PreparedOperation::RebuildLexical,
                        ],
                        Some(status.revision),
                        &c,
                    )?)
                })
            })
            .await?;
        } else if status.revision
            != op
                .base
                .revision
                .checked_add(1)
                .ok_or(IndexError::InvalidJournal)?
        {
            return Err(IndexError::InvalidJournal);
        }
        let store = self.retrieval.clone();
        if !joined(Cancellation::default(), move |c| {
            Ok(store.exchange_matches(&artifact, &c)?)
        })
        .await?
        {
            return Err(IndexError::InvalidJournal);
        }
        let revision = self.status().await?.revision;
        self.journal(lane, move |j, c| {
            j.finish(&op.id, true, op.base.fence, revision, &c)
        })
        .await
    }
}
