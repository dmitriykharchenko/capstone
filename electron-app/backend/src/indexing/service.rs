//! Request orchestration above the durable coordinator. Preparation has no write
//! capability; one whole-job mutex serializes indexing/reset. Attempt diagnostics
//! are explicitly session-only; the v1 journal remains the publication authority.
use super::{CoordinatedIndex, IndexError, PreparedPublication, PublishedPaper, Readiness};
use crate::{
    domain::PaperInfo,
    graph::{
        MetadataExtractor, PaperRecoveryArtifact as GraphArtifact, PreparedPaper as GraphPaper,
        parse_metadata_output,
    },
    papers::{NativePaperRepository, PaperError},
    protocol::ErrorCode,
    providers::EmbeddingClient,
    registry::BackendResult,
    retrieval::{EmbeddedChunk, EmbeddingIdentity, PaperRecoveryArtifact, PreparedPaper},
    runtime::{Cancellation, OperationGroup, RequestContext},
};
use futures_util::future::join_all;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Failure {
    InvalidPdf,
    SourceChanged,
    PreparationFailed,
    Limit,
}
impl Failure {
    fn message(self) -> &'static str {
        match self {
            Self::InvalidPdf => "PDF text could not be extracted.",
            Self::SourceChanged => "PDF changed during indexing. Run indexing again.",
            Self::PreparationFailed => "Embedding or metadata preparation failed.",
            Self::Limit => {
                "Paper exceeds the indexing safety limits. Prior publication was preserved."
            }
        }
    }
}
#[derive(Clone, Serialize)]
struct Attempt {
    status: &'static str,
    error: Option<&'static str>,
    observed_at: String,
}
struct AttemptDrain(Arc<Mutex<BTreeMap<String, Attempt>>>);
impl Drop for AttemptDrain {
    fn drop(&mut self) {
        for attempt in self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values_mut()
        {
            if attempt.status == "preparing" {
                attempt.status = "interrupted";
                attempt.observed_at = chrono::Utc::now().to_rfc3339();
            }
        }
    }
}

#[derive(Clone)]
pub struct NativeIndexer {
    index: CoordinatedIndex,
    papers: NativePaperRepository,
    identity: EmbeddingIdentity,
    embeddings: Arc<dyn EmbeddingClient>,
    metadata: Arc<dyn MetadataExtractor>,
    workers: usize,
    job: Arc<tokio::sync::Mutex<()>>,
    attempts: Arc<Mutex<BTreeMap<String, Attempt>>>,
}
enum Preparation {
    Publish(PreparedPublication),
    Skip(PublishedPaper),
}

impl NativeIndexer {
    /// Trusted composition supplies matching provider/model/mode identity. No
    /// network work or initialization occurs here. Recovery must already be Ready.
    pub fn new(
        index: CoordinatedIndex,
        embeddings: Arc<dyn EmbeddingClient>,
        metadata: Arc<dyn MetadataExtractor>,
        workers: usize,
    ) -> BackendResult<Self> {
        if !(1..=32).contains(&workers) || index.gate().readiness() != Readiness::Ready {
            return Err(ErrorCode::InvalidParams);
        }
        Ok(Self {
            papers: index.papers(),
            identity: index.identity(),
            job: index.request_job(),
            index,
            embeddings,
            metadata,
            workers,
            attempts: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub fn readiness(&self) -> &'static str {
        match self.index.gate().readiness() {
            Readiness::Ready => "ready",
            Readiness::Starting | Readiness::Recovering => "recovering",
            Readiness::Poisoned => "recovery_required",
        }
    }
    fn attempt(&self, id: &str, status: &'static str, failure: Option<Failure>) {
        self.attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id.into(),
                Attempt {
                    status,
                    error: failure.map(Failure::message),
                    observed_at: chrono::Utc::now().to_rfc3339(),
                },
            );
    }
    async fn list(&self, group: &OperationGroup) -> BackendResult<Vec<PaperInfo>> {
        let repo = self.papers.clone();
        group
            .run_preparation("index-list", move |c| async move {
                c.check()?;
                let papers = repo.list_papers().map_err(ErrorCode::from)?;
                c.check()?;
                Ok(papers)
            })
            .await
    }
    async fn prepare(
        &self,
        paper: &PaperInfo,
        c: Cancellation,
        force: bool,
    ) -> Result<Preparation, Failure> {
        let prior = self
            .index
            .published_paper(paper.id.clone())
            .await
            .map_err(|_| Failure::PreparationFailed)?;
        let source = self
            .papers
            .prepare_for_index(&paper.id, &c)
            .map_err(|e| match e {
                PaperError::ChangedDuringRead => Failure::SourceChanged,
                PaperError::TooLarge | PaperError::ExtractionLimit => Failure::Limit,
                _ => Failure::InvalidPdf,
            })?;
        if !force
            && let Some(p) = &prior
            && p.source_sha256 == source.source_sha256
        {
            return Ok(Preparation::Skip(p.clone()));
        }
        c.check().map_err(|_| Failure::PreparationFailed)?;
        let count = source.chunks.len();
        if count > crate::retrieval::MAX_RECOVERY_CHUNKS
            || count.saturating_mul(self.identity.dimension)
                > crate::retrieval::MAX_RECOVERY_VECTOR_VALUES
        {
            return Err(Failure::Limit);
        }
        let graph = if source.chunks.is_empty() {
            GraphArtifact::absent(paper.id.clone())
        } else {
            // Metadata input is bounded before joining; no corpus-sized prompt.
            let text: String = source
                .chunks
                .iter()
                .flat_map(|c| c.text.chars().chain(std::iter::once('\n')))
                .take(crate::graph::MAX_METADATA_TEXT_CHARS)
                .collect();
            let metadata = self
                .metadata
                .extract(text, c.clone())
                .await
                .map_err(|_| Failure::PreparationFailed)?;
            let metadata = parse_metadata_output(&metadata.to_string())
                .map_err(|_| Failure::PreparationFailed)?;
            GraphArtifact::from_prepared(
                &GraphPaper::new(
                    &paper.id,
                    metadata.title(),
                    metadata.authors(),
                    metadata.topics(),
                )
                .map_err(|_| Failure::Limit)?,
            )
        }
        .map_err(|_| Failure::Limit)?;
        let mut chunks = Vec::with_capacity(count);
        for batch in source.chunks.chunks(64) {
            c.check().map_err(|_| Failure::PreparationFailed)?;
            let vectors = self
                .embeddings
                .embed(batch.iter().map(|c| c.text.clone()).collect(), c.clone())
                .await
                .map_err(|_| Failure::PreparationFailed)?;
            if vectors.len() != batch.len() {
                return Err(Failure::PreparationFailed);
            }
            for (chunk, vector) in batch.iter().cloned().zip(vectors) {
                chunks.push(EmbeddedChunk::from_chunk(
                    format!("{}:{}", paper.id, chunk.source.chunk_index),
                    chunk,
                    vector,
                ));
            }
        }
        let retrieval = PreparedPaper::new(paper.id.clone(), self.identity.clone(), chunks)
            .map_err(|_| Failure::PreparationFailed)?;
        let retrieval =
            PaperRecoveryArtifact::from_prepared(&retrieval).map_err(|_| Failure::Limit)?;
        let prepared = PreparedPublication::new(
            source.source_sha256,
            prior.map(|p| p.version_id),
            retrieval,
            graph,
        )
        .map_err(|_| Failure::Limit)?;
        Ok(Preparation::Publish(prepared))
    }
    pub async fn index_papers(
        &self,
        context: RequestContext,
        force: bool,
        streaming: bool,
    ) -> BackendResult<Value> {
        let _job = self.job.try_lock().map_err(|_| ErrorCode::Busy)?;
        let _attempt_drain = AttemptDrain(self.attempts.clone());
        let group = context.operation_group()?;
        group.check()?;
        let papers = self.list(&group).await?;
        // Validate all durable stores before any provider work, including empty runs.
        self.index
            .inspect(vec![], group.preparation_stop())
            .await
            .map_err(ErrorCode::from)?;
        let total = papers.len();
        if streaming {
            context
                .emit("start", json!({"completed":0,"total":total}))
                .await?;
        }
        let (mut completed, mut indexed, mut empty, mut failed, mut skipped) = (0, 0, 0, 0, 0);
        for batch in papers.chunks(self.workers) {
            group.check()?;
            for paper in batch {
                if streaming {
                    context.emit("started", json!({"completed":completed,"total":total,"paper_id":paper.id,"filename":paper.filename})).await?;
                }
                self.attempt(&paper.id, "preparing", None);
            }
            let results = join_all(batch.iter().map(|paper| {
                let this = self.clone();
                let paper = paper.clone();
                let group = group.clone();
                async move {
                    group
                        .run_preparation("index-prepare", move |c| async move {
                            // Domain failures are per-paper outcomes, not hidden group errors.
                            let result = this.prepare(&paper, c.clone(), force).await;
                            c.check()?;
                            Ok(result)
                        })
                        .await
                }
            }))
            .await;
            // join_all retains every admitted group worker even on panic/cancel/error.
            group.check()?;
            if self.readiness() != "ready" {
                return Err(ErrorCode::Internal);
            }
            for (paper, prepared) in batch.iter().zip(results) {
                group.check()?;
                let outcome = match prepared? {
                    Ok(Preparation::Publish(p)) => match self.index.publish(p, &group).await {
                        Ok(p) => Ok((if p.empty { "empty" } else { "indexed" }, p.chunk_count)),
                        Err(IndexError::Conflict) => Err(Failure::SourceChanged),
                        Err(IndexError::Limit) => Err(Failure::Limit),
                        Err(e) => return Err(e.into()),
                    },
                    Ok(Preparation::Skip(prior)) => {
                        let observed = self
                            .index
                            .inspect(vec![paper.id.clone()], group.preparation_stop())
                            .await
                            .map_err(ErrorCode::from)?;
                        if observed[0].2 && observed[0].1.as_ref() == Some(&prior) {
                            Ok(("skipped", prior.chunk_count))
                        } else {
                            Err(Failure::SourceChanged)
                        }
                    }
                    Err(e) => Err(e),
                };
                let (status, count, error) = match outcome {
                    Ok((status, count)) => {
                        self.attempt(&paper.id, status, None);
                        (status, count, None)
                    }
                    Err(error) => {
                        self.attempt(&paper.id, "failed", Some(error));
                        ("failed", 0, Some(error.message()))
                    }
                };
                match status {
                    "indexed" => indexed += 1,
                    "empty" => empty += 1,
                    "skipped" => skipped += 1,
                    _ => failed += 1,
                }
                completed += 1;
                if streaming {
                    context
                        .emit(
                            "progress",
                            json!({"completed":completed,"total":total,"paper_id":paper.id,
                    "filename":paper.filename,"status":status,"chunk_count":count,"error":error}),
                        )
                        .await?;
                }
            }
        }
        self.index
            .inspect(vec![], group.preparation_stop())
            .await
            .map_err(ErrorCode::from)?;
        let result = json!({"completed":completed,"total":total,"indexed":indexed,"empty":empty,"failed":failed,"skipped":skipped});
        let _seal = group.seal_completion()?;
        Ok(if streaming {
            result
        } else {
            json!({"enabled":true,"indexed":indexed,"summary":result})
        })
    }
    pub async fn statuses(&self, context: &RequestContext) -> BackendResult<Value> {
        let group = context.operation_group()?;
        let papers = self.list(&group).await?;
        let observation = self
            .index
            .inspect(
                papers.iter().map(|p| p.id.clone()).collect(),
                group.preparation_stop(),
            )
            .await;
        let statuses = match observation {
            Ok(rows) => {
                let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
                rows.into_iter().map(|(id,p,current)| {
                    let state = match &p { None => "not_indexed", Some(_) if !current => "stale", Some(p) if p.empty => "empty", Some(_) => "indexed" };
                    json!({"paper_id":id,"filename":format!("{id}.pdf"),"status":state,"chunk_count":p.as_ref().map_or(0, |p|p.chunk_count),
                        "version_id":p.map(|p|p.version_id),"attempt":attempts.get(&id),"attempt_scope":"session"})
                }).collect::<Vec<_>>()
            }
            Err(IndexError::Cancelled) => return Err(ErrorCode::Cancelled),
            Err(_) => Vec::new(),
        };
        Ok(json!({"readiness":self.readiness(),"statuses":statuses}))
    }
    pub async fn bm25_status(&self, context: &RequestContext) -> BackendResult<Value> {
        let status = self.statuses(context).await?;
        let rows = status["statuses"].as_array().ok_or(ErrorCode::Internal)?;
        let covered: Vec<_> = rows.iter().filter(|p| p["status"] == "indexed").collect();
        let count: u64 = covered
            .iter()
            .map(|p| p["chunk_count"].as_u64().unwrap_or(0))
            .sum();
        Ok(
            json!({"readiness":status["readiness"],"built":status["readiness"] == "ready","chunk_count":count,
            "paper_ids":covered.iter().map(|p|p["paper_id"].clone()).collect::<Vec<_>>()}),
        )
    }
    pub async fn reset(&self, context: &RequestContext) -> BackendResult<Value> {
        let _job = self.job.try_lock().map_err(|_| ErrorCode::Busy)?;
        self.index
            .reset(&context.operation_group()?)
            .await
            .map_err(ErrorCode::from)?;
        self.attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(
            json!({"ok":true,"reset":{"vector":true,"bm25":true,"statuses":true,"graph":true},"preserved":{"pdfs":true,"history":true}}),
        )
    }
}
