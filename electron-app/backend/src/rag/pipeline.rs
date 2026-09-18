use std::{future::Future, sync::Arc, time::Duration};

use serde_json::{Value, json};

use crate::{
    config::AppConfig,
    domain::{Message, RagResult, RetrievalScope, RetrievalStrategy},
    providers::ModelClient,
    registry::BackendFuture,
    retrieval::Retriever,
    runtime::{Cancellation, RequestContext},
};

use super::{
    HopEvidence, MAX_SOURCES, MAX_TOTAL_HOPS, PAPER_CHUNK_LIMIT, RagError, RagErrorKind, RagModel,
    RagPipeline, RagRun, RagStage, RunMode, evidence, model::FoundationModel, normalization::blank,
    prompts, validation,
};

#[derive(Clone)]
pub struct AdaptiveRag {
    model: Arc<dyn RagModel>,
    retriever: Arc<dyn Retriever>,
    system_prompt: String,
}

impl AdaptiveRag {
    /// Compatibility constructor for the frozen foundation trait. Its ErrorCode
    /// cannot recover provider truncation/auth/rate-limit distinctions. Production
    /// should use with_model/from_config with HttpModelClient's typed RagModel impl.
    pub fn new(
        model: Arc<dyn ModelClient>,
        retriever: Arc<dyn Retriever>,
        system_prompt: String,
    ) -> Result<Self, RagError> {
        Self::with_model(Arc::new(FoundationModel(model)), retriever, system_prompt)
    }

    pub fn with_model(
        model: Arc<dyn RagModel>,
        retriever: Arc<dyn Retriever>,
        system_prompt: String,
    ) -> Result<Self, RagError> {
        if blank(&system_prompt) {
            return Err(RagError::new(RagStage::Input, RagErrorKind::InvalidInput));
        }
        evidence::bounded(&system_prompt, RagStage::Input)?;
        Ok(Self {
            model,
            retriever,
            system_prompt,
        })
    }

    /// Reads ONLY the supplied startup config; never consults env/files/secrets.
    pub fn from_config(
        model: Arc<dyn RagModel>,
        retriever: Arc<dyn Retriever>,
        config: &AppConfig,
    ) -> Result<Self, RagError> {
        Self::with_model(
            model,
            retriever,
            config
                .system_prompt
                .as_ref()
                .map(|s| s.expose_secret())
                .unwrap_or(prompts::DEFAULT_SYSTEM)
                .to_owned(),
        )
    }

    /// The single Chat/Evals algorithm. No persistence or terminal publication.
    /// Caller retains trace only as needed and maps result to its public DTO.
    pub async fn run_stateless(
        &self,
        question: String,
        mut history: Vec<Message>,
        mode: RunMode,
    ) -> Result<RagRun, RagError> {
        let cancellation = mode.cancellation();
        check(&cancellation, RagStage::Input)?;
        if blank(&question) {
            return Err(RagError::new(RagStage::Input, RagErrorKind::InvalidInput));
        }
        for item in &mut history {
            item.role = match item.role.as_str() {
                "human" => "user",
                "ai" => "assistant",
                role => role,
            }
            .into();
            if !matches!(
                item.role.as_str(),
                "user" | "assistant" | "system" | "developer"
            ) || blank(&item.content)
            {
                return Err(RagError::new(RagStage::Input, RagErrorKind::InvalidInput));
            }
            item.exchange_id = None;
            item.sources.clear();
        }
        evidence::bounded(&(&question, &history), RagStage::Input)?;
        let raw = self
            .complete(
                prompts::planner(&question, &history),
                &cancellation,
                RagStage::Planner,
            )
            .await?;
        let initial = validation::plan(&raw)?;
        emit(
            &mode,
            RagStage::Planner,
            "plan",
            json!({"subqueries": descriptors(&initial, 0)}),
        )
        .await?;
        let mut hops = Vec::with_capacity(MAX_TOTAL_HOPS);
        for query in &initial {
            hops.push(
                self.hop(
                    &question,
                    query,
                    hops.len() + 1,
                    initial.len(),
                    false,
                    &mode,
                )
                .await?,
            );
        }
        emit(
            &mode,
            RagStage::Coverage,
            "coverage",
            json!({"status":"reviewing","total":initial.len(),"subqueries":[]}),
        )
        .await?;
        let raw = self
            .complete(
                prompts::coverage(&question, &hops),
                &cancellation,
                RagStage::Coverage,
            )
            .await?;
        let followups = validation::coverage(&raw, &initial)?;
        let total = initial.len() + followups.len();
        emit(
            &mode,
            RagStage::Coverage,
            "coverage",
            json!({
                "status": if followups.is_empty() { "complete" } else { "followup" },
                "total":total,"subqueries":descriptors(&followups, initial.len()),
            }),
        )
        .await?;
        for query in &followups {
            hops.push(
                self.hop(&question, query, hops.len() + 1, total, true, &mode)
                    .await?,
            );
        }
        if hops.len() > MAX_TOTAL_HOPS
            || hops.iter().map(|h| h.chunks.len()).sum::<usize>() > MAX_SOURCES
        {
            return Err(RagError::new(
                RagStage::Coverage,
                RagErrorKind::InvalidCoverage,
            ));
        }
        let messages = prompts::composer(&self.system_prompt, &question, &history, &hops);
        let stage = RagStage::Composer;
        evidence::bounded(&messages, stage)?;
        check(&cancellation, stage)?;
        let reply = match &mode {
            RunMode::Silent(_) => self.complete(messages, &cancellation, stage).await?,
            RunMode::Public(context) => {
                let reply = wait(
                    self.model.stream_final(messages, context.clone()),
                    &cancellation,
                    stage,
                )
                .await?
                .map_err(|error| error.at(stage))?;
                validate_text(&reply, stage)?;
                reply
            }
        };
        let chunks = evidence::aggregate(&hops);
        let sources = chunks.iter().map(|chunk| chunk.source.clone()).collect();
        let result = RagResult {
            reply,
            sources,
            chunks,
        };
        evidence::bounded(&result, stage)?;
        check(&cancellation, stage)?;
        Ok(RagRun {
            result,
            initial_count: initial.len(),
            followup_subqueries: followups,
            hops,
        })
    }

    async fn complete(
        &self,
        messages: Vec<Message>,
        cancellation: &Cancellation,
        stage: RagStage,
    ) -> Result<String, RagError> {
        check(cancellation, stage)?;
        evidence::bounded(&messages, stage)?;
        let text = wait(
            self.model.complete(messages, cancellation.clone()),
            cancellation,
            stage,
        )
        .await?
        .map_err(|error| error.at(stage))?;
        validate_text(&text, stage)?;
        Ok(text)
    }

    #[allow(clippy::too_many_arguments)]
    async fn hop(
        &self,
        question: &str,
        query: &str,
        index: usize,
        total: usize,
        followup: bool,
        mode: &RunMode,
    ) -> Result<HopEvidence, RagError> {
        let stage = if followup {
            RagStage::FollowupRetrieval
        } else {
            RagStage::Retrieval
        };
        let cancellation = mode.cancellation();
        let descriptor =
            json!({"id":format!("hop-{index}"),"index":index,"total":total,"query":query});
        emit(mode, stage, "subquery", descriptor.clone()).await?;
        check(&cancellation, stage)?;
        let chunks = wait(
            self.retriever.retrieve(
                query.into(),
                RetrievalStrategy::Hybrid,
                RetrievalScope::ChatCandidates,
                PAPER_CHUNK_LIMIT,
                cancellation.clone(),
            ),
            &cancellation,
            stage,
        )
        .await?
        .map_err(|code| RagError::backend(stage, code))?;
        let (context, chunks) = evidence::prepare(chunks, stage)?;
        let stage = if followup {
            RagStage::FollowupSubanswer
        } else {
            RagStage::Subanswer
        };
        let answer = self
            .complete(
                prompts::subanswer(&self.system_prompt, question, query, &context),
                &cancellation,
                stage,
            )
            .await?;
        let mut data = descriptor;
        data["answer"] = json!(answer);
        data["sources"] = json!(chunks.iter().map(|chunk| &chunk.source).collect::<Vec<_>>());
        emit(mode, stage, "subanswer", data).await?;
        Ok(HopEvidence {
            subquery: query.into(),
            answer,
            context,
            chunks,
        })
    }
}

impl RagPipeline for AdaptiveRag {
    fn run(
        &self,
        question: String,
        history: Vec<Message>,
        context: RequestContext,
    ) -> BackendFuture<RagResult> {
        let pipeline = self.clone();
        Box::pin(async move {
            pipeline
                .run_stateless(question, history, RunMode::Public(context))
                .await
                .map(|run| run.result)
                .map_err(Into::into)
        })
    }
}

fn check(cancellation: &Cancellation, stage: RagStage) -> Result<(), RagError> {
    cancellation
        .check()
        .map_err(|code| RagError::backend(stage, code))
}

// Cancellation also works for injected implementations while awaiting a stage.
// No spawned/detached worker: dropping this future drops its in-flight operation.
async fn wait<T>(
    future: impl Future<Output = T>,
    cancellation: &Cancellation,
    stage: RagStage,
) -> Result<T, RagError> {
    check(cancellation, stage)?;
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => { check(cancellation, stage)?; return Ok(result); }
            _ = tokio::time::sleep(Duration::from_millis(10)) => check(cancellation, stage)?,
        }
    }
}

fn validate_text(text: &str, stage: RagStage) -> Result<(), RagError> {
    if blank(text) {
        let kind = match stage {
            RagStage::Planner => RagErrorKind::InvalidPlan,
            RagStage::Coverage => RagErrorKind::InvalidCoverage,
            _ => RagErrorKind::EmptyResponse,
        };
        return Err(RagError::new(stage, kind));
    }
    evidence::bounded(&text, stage)
}

fn descriptors(queries: &[String], offset: usize) -> Vec<Value> {
    queries
        .iter()
        .enumerate()
        .map(|(position, query)| {
            let index = offset + position + 1;
            json!({"id":format!("hop-{index}"),"index":index,"query":query})
        })
        .collect()
}

async fn emit(
    mode: &RunMode,
    stage: RagStage,
    event: &'static str,
    data: Value,
) -> Result<(), RagError> {
    let cancellation = mode.cancellation();
    check(&cancellation, stage)?;
    // Validate both modes identically so an oversized progress payload cannot
    // succeed only in Evals. Silent still never receives/publishes a context.
    evidence::bounded(&data, stage)?;
    if let RunMode::Public(context) = mode {
        wait(context.emit(event, data), &cancellation, stage)
            .await?
            .map_err(|code| RagError::backend(stage, code))?;
    }
    check(&cancellation, stage)
}
