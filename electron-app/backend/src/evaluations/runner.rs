use super::{
    ArtifactStore, CaseError, CaseResult, EvaluationArtifact, EvaluationRequest, EvaluationService,
    JudgeTemplate,
    contracts::{MAX_ARTIFACT_BYTES, bounded_result},
    parse_judge_output, validate_test_cases,
};
use crate::{
    config::{AppConfig, Configuration, StoragePaths},
    domain::{EvaluationCase, RetrievalStrategy},
    protocol::{ErrorCode, MAX_FRAME_BYTES},
    providers::ProviderError,
    rag::{AdaptiveRag, ModelFailure, RagError, RagErrorKind, RagModel, RagStage, RunMode},
    registry::{BackendFuture, BackendResult},
    retrieval::Retriever,
    runtime::{Cancellation, RequestContext},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct NativeEvaluationService {
    pipeline: AdaptiveRag,
    judge: Arc<dyn RagModel>,
    template: JudgeTemplate,
    store: ArtifactStore,
}
impl NativeEvaluationService {
    pub fn new(
        pipeline: AdaptiveRag,
        judge: Arc<dyn RagModel>,
        paths: &StoragePaths,
    ) -> BackendResult<Self> {
        Ok(Self {
            pipeline,
            judge,
            template: JudgeTemplate::embedded()?,
            store: ArtifactStore::from_paths(paths)?,
        })
    }
    pub fn from_config(
        model: Arc<dyn RagModel>,
        retriever: Arc<dyn Retriever>,
        config: &AppConfig,
    ) -> BackendResult<Self> {
        let pipeline =
            AdaptiveRag::from_config(model.clone(), retriever, config).map_err(ErrorCode::from)?;
        Self::new(pipeline, model, config.paths())
    }
    pub async fn run_request(
        &self,
        request: EvaluationRequest,
        context: RequestContext,
    ) -> BackendResult<Value> {
        request.validate()?;
        self.execute(request.test_cases, context).await
    }
    async fn execute(
        &self,
        cases: Vec<EvaluationCase>,
        context: RequestContext,
    ) -> BackendResult<Value> {
        let token = context.cancellation();
        token.check()?;
        validate_test_cases(&cases)?;
        // Fail-fast, no waiting queue: one run per pinned output directory across
        // services/processes, acquired before progress/provider calls.
        let lease = self.store.acquire()?;
        token.check()?;
        let total = cases.len();
        context
            .emit(
                "run_start",
                json!({"retrieval_type":"hybrid","total":total}),
            )
            .await?;
        let mut results = Vec::with_capacity(total);
        let mut retained = 0;
        for (index, case) in cases.iter().enumerate() {
            token.check()?;
            let case_index = index + 1;
            context.emit("case_start", json!({"case_index":case_index,"total":total,"retrieval_type":"hybrid","input":case})).await?;
            let result = self.run_case(case_index, case.clone(), &token).await?;
            token.check()?;
            result.validate(total)?;
            let event = if result.error.is_none() {
                "case_success"
            } else {
                "case_failure"
            };
            let data = json!({"completed":case_index,"total":total,"result":result});
            // Check even non-streaming callers; RequestContext bounds streaming envelopes.
            let size = serde_json::to_vec(&data)
                .map_err(|_| ErrorCode::Internal)?
                .len();
            retained += size;
            if size > MAX_FRAME_BYTES - 256 || retained > MAX_ARTIFACT_BYTES / 2 {
                return Err(ErrorCode::ResponseTooLarge);
            }
            context.emit(event, data).await?;
            results.push(result);
        }
        token.check()?;
        let artifact = EvaluationArtifact::new(cases, results)?;
        let complete = bounded_result(&artifact.complete())?;
        let bytes = artifact.serialize()?;
        token.check()?;
        lease.publish(&bytes, &token)?;
        // No awaits after publication; runtime retains the sticky seal through output.
        Ok(complete)
    }
    async fn run_case(
        &self,
        index: usize,
        input: EvaluationCase,
        token: &Cancellation,
    ) -> BackendResult<CaseResult> {
        let mut result = CaseResult {
            case_index: index,
            input,
            retrieval_strategy: RetrievalStrategy::Hybrid,
            retrieved_chunks: vec![],
            generated_answer: None,
            judge_scores: None,
            error: None,
        };
        let run = match self
            .pipeline
            .run_stateless(
                result.input.question.clone(),
                vec![],
                RunMode::Silent(token.clone()),
            )
            .await
        {
            Ok(run) => run,
            Err(error) => {
                abort_rag(error)?;
                result.error = Some(CaseError::new(
                    stage(error.stage),
                    matches!(
                        error.kind,
                        RagErrorKind::Provider(ProviderError::IncompleteResponse)
                    ),
                ));
                return Ok(result);
            }
        };
        token.check()?;
        result.retrieved_chunks = run.result.chunks.into_iter().map(Into::into).collect();
        result.generated_answer = Some(run.result.reply);
        let messages = self.template.render(
            &result.input,
            &result.retrieved_chunks,
            result.generated_answer.as_deref().unwrap_or_default(),
        )?;
        token.check()?;
        let future = self.judge.complete(messages, token.clone());
        tokio::pin!(future);
        let response = loop {
            tokio::select! {
                result = &mut future => break result,
                _ = tokio::time::sleep(Duration::from_millis(10)) => token.check()?,
            }
        };
        token.check()?;
        match response {
            Ok(text) => match parse_judge_output(&text) {
                Ok(scores) => result.judge_scores = Some(scores),
                Err(ErrorCode::ResponseTooLarge) => return Err(ErrorCode::ResponseTooLarge),
                Err(_) => result.error = Some(CaseError::new("judge", false)),
            },
            Err(error) => {
                let kind = match error {
                    ModelFailure::Provider(e) => RagErrorKind::Provider(e),
                    ModelFailure::Backend(e) => RagErrorKind::Backend(e),
                };
                abort_rag(RagError::new(RagStage::Composer, kind))?;
                result.error = Some(CaseError::new(
                    "judge",
                    matches!(
                        error,
                        ModelFailure::Provider(ProviderError::IncompleteResponse)
                    ),
                ));
            }
        }
        Ok(result)
    }
}
impl EvaluationService for NativeEvaluationService {
    fn run(&self, cases: Vec<EvaluationCase>, context: RequestContext) -> BackendFuture<Value> {
        let service = self.clone();
        Box::pin(async move { service.execute(cases, context).await })
    }
}
fn abort_rag(error: RagError) -> BackendResult<()> {
    match error.kind {
        RagErrorKind::Cancelled
        | RagErrorKind::Provider(ProviderError::Cancelled)
        | RagErrorKind::Backend(ErrorCode::Cancelled) => Err(ErrorCode::Cancelled),
        RagErrorKind::TooLarge
        | RagErrorKind::Provider(ProviderError::TooLarge)
        | RagErrorKind::Backend(ErrorCode::ResponseTooLarge) => Err(ErrorCode::ResponseTooLarge),
        _ => Ok(()),
    }
}
fn stage(stage: RagStage) -> &'static str {
    match stage {
        RagStage::Input => "evaluation",
        RagStage::Planner => "planner",
        RagStage::Retrieval => "retrieval",
        RagStage::Subanswer => "subanswer",
        RagStage::Coverage => "coverage",
        RagStage::FollowupRetrieval => "followup_retrieval",
        RagStage::FollowupSubanswer => "followup_subanswer",
        RagStage::Composer => "composer",
    }
}
