use super::diagnostics::{Diagnostic, Stage};
use crate::{
    history::{Admission, CommittedTurn, HistoryError, SqliteHistory, TurnRequest},
    indexing::CoordinatedIndex,
    protocol::{ErrorCode, MAX_FRAME_BYTES},
    rag::RagPipeline,
    registry::BackendResult,
    retrieval::{HistorySearchRequest, NativeRetriever, RetrievalError},
    runtime::{Cancellation, RequestContext},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, watch};

#[derive(Default)]
struct Admissions {
    turns: HashMap<String, (String, watch::Receiver<bool>)>,
    sessions: HashMap<String, watch::Receiver<bool>>,
}
struct Completed(watch::Sender<bool>);
impl Drop for Completed {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

#[derive(Clone)]
pub struct NativeChat {
    pub(super) history: SqliteHistory,
    pub(super) index: CoordinatedIndex,
    retriever: NativeRetriever,
    pipeline: Arc<dyn RagPipeline>,
    admissions: Arc<Mutex<Admissions>>,
}
impl NativeChat {
    pub fn new(
        history: SqliteHistory,
        index: CoordinatedIndex,
        retriever: NativeRetriever,
        pipeline: Arc<dyn RagPipeline>,
    ) -> Self {
        Self {
            history,
            index,
            retriever,
            pipeline,
            admissions: Arc::new(Mutex::new(Admissions::default())),
        }
    }
    pub async fn turn(
        &self,
        request: TurnRequest,
        context: RequestContext,
    ) -> BackendResult<Value> {
        let mut diagnostic = Diagnostic::new();
        let result = self.turn_inner(request, context, &mut diagnostic).await;
        diagnostic.error = result.as_ref().err().copied();
        result
    }
    async fn turn_inner(
        &self,
        request: TurnRequest,
        context: RequestContext,
        diagnostic: &mut Diagnostic,
    ) -> BackendResult<Value> {
        let fingerprint = request.fingerprint().map_err(history_error)?;
        let group = context.operation_group()?;
        let mut admission = self.admissions.lock().await;
        group.check()?;
        admission.turns.retain(|_, (_, r)| !*r.borrow());
        admission.sessions.retain(|_, r| !*r.borrow());
        if let Some((expected, done)) = admission.turns.get(&request.turn_id) {
            if expected != &fingerprint {
                return Err(ErrorCode::InvalidParams);
            }
            let done = done.clone();
            drop(admission);
            wait(done, &context.cancellation()).await?;
            let result = self.reconcile(&request, &context, false).await?;
            let _seal = group.seal_completion()?;
            return Ok(result);
        }
        let h = self.history.clone();
        let r = request.clone();
        let admitted = group
            .run_mutation("chat-admission", move |c| async move {
                Ok(h.admit_turn(&r, &c))
            })
            .await?
            .map_err(history_error)?;
        if matches!(admitted, Admission::Existing) {
            drop(admission);
            let result = self.reconcile(&request, &context, false).await?;
            let _seal = group.seal_completion()?;
            return Ok(result);
        }
        let (send, receive) = watch::channel(false);
        let _completed = Completed(send);
        let previous = admission
            .sessions
            .insert(request.session_id.clone(), receive.clone());
        admission
            .turns
            .insert(request.turn_id.clone(), (fingerprint, receive));
        drop(admission);
        if let Some(mut previous) = previous {
            // A cancelled queued turn still drains its predecessor before releasing
            // its successor; otherwise cancellation could let later turns overtake.
            while !*previous.borrow() {
                previous
                    .changed()
                    .await
                    .map_err(|_| ErrorCode::OutcomeUnknown)?;
            }
        }
        group.check()?;
        diagnostic.stage = Stage::History;
        let h = self.history.clone();
        let session = request.session_id.clone();
        let messages = group
            .run_preparation("chat-history", move |_| async move {
                h.get_messages(&session).map_err(history_error)
            })
            .await?;
        let pipeline = self.pipeline.clone();
        diagnostic.stage = Stage::Rag;
        let question = request.message.clone();
        let ctx = context.clone();
        let gate = self.index.gate().clone();
        let index = self.index.clone();
        let result = group
            .run_preparation("chat-rag", move |_| async move {
                let _read = gate.read().await.map_err(|_| ErrorCode::Unavailable)?;
                index
                    .verify_read()
                    .await
                    .map_err(|_| ErrorCode::Unavailable)?;
                pipeline.run(question, messages, ctx).await
            })
            .await?;
        if result.reply.trim().is_empty() {
            return Err(ErrorCode::Internal);
        }
        // Include the longest status/warning and maximum routing ID before the seal.
        bounded(
            &json!({"session_id":request.session_id,"turn_id":request.turn_id,"exchange_id":i64::MAX,
            "reply":result.reply,"sources":result.sources,"chat_mode":"native","indexed":false,
            "history_index_status":"recovery_required","history_index_status_observed_at":{"reset_epoch":u64::MAX,"retrieval_revision":i64::MAX},
            "warning":WARNING}),
        )?;
        let h = self.history.clone();
        let r = request.clone();
        diagnostic.stage = Stage::CanonicalCommit;
        let committed = group
            .run_authoritative_mutation(move |c| async move {
                Ok(h.append_turn(&r, &result.reply, &result.sources, &c))
            })
            .await
            .map_err(|e| {
                if e == ErrorCode::Cancelled {
                    e
                } else {
                    ErrorCode::OutcomeUnknown
                }
            })?;
        if committed.is_err() {
            if !context.cancellation().is_committing() {
                return Err(committed
                    .err()
                    .map(history_error)
                    .unwrap_or(ErrorCode::Internal));
            }
            return Err(ErrorCode::OutcomeUnknown);
        }
        let canonical = self.canonical(&request, &context, true).await?;
        diagnostic.stage = Stage::DerivedIndex;
        // All children drain before result construction. Ordinary derived failure
        // never rolls back canonical truth; uncontained panic stays UNKNOWN.
        let failed = self
            .index
            .index_chat_exchange(canonical, self.retriever.clone(), &group)
            .await
            .is_err();
        diagnostic.stage = Stage::Reconcile;
        let result = self.reconcile(&request, &context, true).await?;
        let mut result = result;
        if failed && result["indexed"] != true && result["history_index_status"] == "pending" {
            result["history_index_status"] = json!("failed");
        }
        let _seal = group
            .seal_completion()
            .map_err(|_| ErrorCode::OutcomeUnknown)?;
        bounded(&result).map_err(|_| ErrorCode::OutcomeUnknown)?;
        Ok(result)
    }
    async fn canonical(
        &self,
        r: &TurnRequest,
        context: &RequestContext,
        committed: bool,
    ) -> BackendResult<CommittedTurn> {
        let h = self.history.clone();
        let r = r.clone();
        let work = move |_| async move { Ok(h.get_turn_outcome(&r)) };
        let group = context.operation_group()?;
        let result = if committed {
            group
                .run_derived(work)
                .await
                .map_err(|_| ErrorCode::OutcomeUnknown)?
                .map_err(|_| ErrorCode::OutcomeUnknown)?
        } else {
            group.run_preparation("chat-reconcile", work).await?
        };
        match result {
            Ok(Some(c)) => Ok(c),
            Err(HistoryError::Conflict) => Err(ErrorCode::InvalidParams),
            _ => Err(ErrorCode::OutcomeUnknown),
        }
    }
    async fn reconcile(
        &self,
        r: &TurnRequest,
        context: &RequestContext,
        committed: bool,
    ) -> BackendResult<Value> {
        let canonical = self.canonical(r, context, committed).await?;
        let (status, observed, indexed) = self.index_status(canonical.intent.exchange_id).await;
        // Re-read AFTER derived inspection as well; never present cached evidence.
        let verified = self.canonical(r, context, committed).await?;
        if canonical.intent != verified.intent {
            return Err(ErrorCode::OutcomeUnknown);
        }
        Ok(
            json!({"session_id":r.session_id,"turn_id":r.turn_id,"exchange_id":canonical.intent.exchange_id,
            "reply":verified.exchange.assistant_message.content,"sources":verified.exchange.assistant_message.sources,
            "chat_mode":"native","indexed":indexed,"history_index_status":status,
            "history_index_status_observed_at":observed,"warning":if indexed { None } else { Some(WARNING) }}),
        )
    }
    pub(super) async fn index_status(&self, id: i64) -> (&'static str, Value, bool) {
        match self.index.observe_exchange(id).await {
            Ok(o) => (
                if o.indexed { "indexed" } else { "pending" },
                json!({"reset_epoch":o.reset_epoch,"retrieval_revision":o.retrieval_revision}),
                o.indexed,
            ),
            Err(_) => ("blocked", Value::Null, false),
        }
    }
    pub(super) async fn search(
        &self,
        request: HistorySearchRequest,
        c: Cancellation,
    ) -> BackendResult<Value> {
        let _guard = self
            .index
            .gate()
            .read()
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        self.index
            .verify_read()
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        let matches = self.retriever.search_history_typed(request, c).await;
        if matches!(
            matches,
            Err(RetrievalError::History
                | RetrievalError::CanonicalMissing
                | RetrievalError::InvalidStore)
        ) {
            self.index.poison_derived();
        }
        Ok(
            json!({"enabled":true,"matches":matches.map_err(ErrorCode::from)?.into_iter().map(|e| json!({"exchange_id":e.exchange_id,"session_id":e.session_id,"user_message":e.user_message.content,"assistant_message":e.assistant_message.content})).collect::<Vec<_>>()}),
        )
    }
}
const WARNING: &str = "Reply saved in history. History search indexing is incomplete; no reply was rolled back. Reindex requires an explicit action.";
fn bounded(v: &Value) -> BackendResult<()> {
    if serde_json::to_vec(v)
        .map_err(|_| ErrorCode::Internal)?
        .len()
        > MAX_FRAME_BYTES - 1024
    {
        Err(ErrorCode::ResponseTooLarge)
    } else {
        Ok(())
    }
}
pub(super) fn history_error(e: HistoryError) -> ErrorCode {
    match e {
        HistoryError::Conflict | HistoryError::InvalidInput => ErrorCode::InvalidParams,
        _ => e.into(),
    }
}
async fn wait(mut done: watch::Receiver<bool>, c: &Cancellation) -> BackendResult<()> {
    loop {
        c.check()?;
        if *done.borrow() {
            return Ok(());
        }
        tokio::select! { result = done.changed() => { if result.is_err() { return Err(ErrorCode::OutcomeUnknown); } }, _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {} }
    }
}
