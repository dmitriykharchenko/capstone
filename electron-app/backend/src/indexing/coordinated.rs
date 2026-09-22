//! Infrastructure only: no indexing loop, providers, IPC handlers, or Chat policy.
//! The journal activation COMMIT is publication, never an event/phase response.
use super::{
    DerivedStoreGate, IndexError, IndexResult,
    journal::{self, Journal, Kind, Meta, Operation, PaperOp, Phase, Version},
    worker::joined,
};
use crate::{
    config::StoragePaths,
    graph::{CozoGraphStore, Fence, PaperRecoveryArtifact as GraphArtifact, Transition},
    papers::NativePaperRepository,
    retrieval::{
        EmbeddingIdentity, IndexStatus, LexicalStatus, PaperRecoveryArtifact as RetrievalArtifact,
        PreparedOperation, PreparedReset, VectorStore,
    },
    runtime::{Cancellation, Finalization, OperationGroup, ResetStep},
};
use std::future::Future;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[derive(Clone)]
pub struct PreparedPublication {
    version: Version,
    expected_version: Option<String>,
}
impl PreparedPublication {
    /// The caller prepares bounded same-descriptor content outside the gate. The
    /// source is read-only rehashed and the captured active version is rechecked
    /// under the write gate; these observations cannot lock an external editor.
    pub fn new(
        source_sha256: String,
        expected_version: Option<String>,
        retrieval: RetrievalArtifact,
        graph: GraphArtifact,
    ) -> IndexResult<Self> {
        let version = Version {
            id: uuid::Uuid::new_v4().to_string(),
            source: source_sha256,
            retrieval: String::from_utf8(retrieval.to_json()?)
                .map_err(|_| IndexError::InvalidJournal)?,
            graph: String::from_utf8(graph.to_json()?).map_err(|_| IndexError::InvalidJournal)?,
        };
        version.validate()?;
        Ok(Self {
            version,
            expected_version,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedPaper {
    pub version_id: String,
    pub source_sha256: String,
    pub empty: bool,
    pub chunk_count: usize,
}
impl PublishedPaper {
    fn from_version(v: &Version) -> IndexResult<Self> {
        let r = v.validate()?.0;
        let chunk_count = match r.restore_operation(r.identity())? {
            PreparedOperation::ReplacePaper(p) => p.chunks().len(),
            _ => 0,
        };
        Ok(Self {
            version_id: v.id.clone(),
            source_sha256: v.source.clone(),
            empty: !r.has_rows(),
            chunk_count,
        })
    }
}

#[derive(Clone)]
pub struct CoordinatedIndex {
    gate: DerivedStoreGate,
    request_job: std::sync::Arc<tokio::sync::Mutex<()>>,
    journal: Journal,
    retrieval: VectorStore,
    graph: CozoGraphStore,
    papers: NativePaperRepository,
}
#[derive(Clone, Copy)]
enum Lane<'a> {
    Startup,
    Request(&'a OperationGroup),
    Compensation(&'a OperationGroup),
    Reset(&'a Finalization, ResetStep),
}
impl Lane<'_> {
    async fn run<T, F, Fut>(self, work: F) -> IndexResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Cancellation) -> Fut + Send + 'static,
        Fut: Future<Output = IndexResult<T>> + Send + 'static,
    {
        match self {
            Self::Startup => work(Cancellation::default()).await,
            Self::Request(g) => g
                .run_mutation("index-transition", |c| async move {
                    work(c).await.map_err(Into::into)
                })
                .await
                .map_err(Into::into),
            Self::Compensation(g) => g
                .run_compensation("index-reconcile", |c| async move {
                    work(c).await.map_err(Into::into)
                })
                .await
                .map_err(Into::into),
            Self::Reset(f, s) => f
                .run_step(s, |c| async move { work(c).await.map_err(Into::into) })
                .await
                .map_err(Into::into),
        }
    }
}
impl CoordinatedIndex {
    /// Trusted startup only, with a distinct startup token for each initializer.
    /// Opening never advertises Ready; recover must succeed before assembly.
    pub async fn open(paths: &StoragePaths, identity: EmbeddingIdentity) -> IndexResult<Self> {
        let writable = [
            &paths.index_state_database,
            &paths.vector_database,
            &paths.graph_database,
            &paths.history_database,
        ];
        for (i, p) in writable.iter().enumerate() {
            if p.parent() != Some(paths.data_root.as_path()) || writable[..i].contains(p) {
                return Err(IndexError::InvalidPath);
            }
        }
        let mut reserved = vec![paths.index_state_database.with_extension("sqlite3.lock")];
        reserved.extend(["-journal", "-wal", "-shm"].iter().map(|suffix| {
            std::path::PathBuf::from(format!("{}{suffix}", paths.index_state_database.display()))
        }));
        if writable.iter().any(|p| reserved.contains(p)) {
            return Err(IndexError::InvalidPath);
        }
        if paths.papers.starts_with(&paths.data_root) || paths.data_root.starts_with(&paths.papers)
        {
            return Err(IndexError::InvalidPath);
        }
        let papers =
            NativePaperRepository::new(&paths.papers).map_err(|_| IndexError::InvalidPath)?;
        let root = paths.data_root.clone();
        let path = paths.index_state_database.clone();
        let journal = joined(Cancellation::default(), move |c| {
            Journal::open(&root, &path, &c)
        })
        .await?;
        let retrieval = VectorStore::new(&paths.data_root, &paths.vector_database, identity)?;
        let r = retrieval.clone();
        joined(Cancellation::default(), move |c| {
            r.initialize(&c).map_err(Into::into)
        })
        .await?;
        let graph = CozoGraphStore::open(
            paths.data_root.clone(),
            paths.graph_database.clone(),
            Cancellation::default(),
        )
        .await?;
        Ok(Self {
            gate: DerivedStoreGate::new(),
            request_job: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            journal,
            retrieval,
            graph,
            papers,
        })
    }
    pub fn gate(&self) -> &DerivedStoreGate {
        &self.gate
    }
    pub(super) fn papers(&self) -> NativePaperRepository {
        self.papers.clone()
    }
    pub(super) fn request_job(&self) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.request_job.clone()
    }
    pub(super) fn identity(&self) -> EmbeddingIdentity {
        self.retrieval.identity().clone()
    }
    /// One verified authority/store observation. Source freshness is a read-only
    /// observation, not a lease against external editors. No providers or repair.
    pub async fn inspect(
        &self,
        ids: Vec<String>,
        cancellation: Cancellation,
    ) -> IndexResult<Vec<(String, Option<PublishedPaper>, bool)>> {
        cancellation.check()?;
        let _guard = self.gate.read().await?;
        if let Err(error) = self.verify_ready().await {
            self.gate.poison();
            return Err(error);
        }
        let mut result = Vec::new();
        for id in ids {
            cancellation.check()?;
            let lookup = id.clone();
            let version = self
                .journal(Lane::Startup, move |j, _| j.active(&lookup))
                .await;
            let version = match version {
                Ok(v) => v,
                Err(e) => {
                    self.gate.poison();
                    return Err(e);
                }
            };
            let published = version
                .as_ref()
                .map(PublishedPaper::from_version)
                .transpose()?;
            let current = if let Some(v) = &published {
                let repo = self.papers.clone();
                let paper = id.clone();
                let expected = v.source_sha256.clone();
                joined(cancellation.clone(), move |c| {
                    match repo.read_pdf_bytes(&paper, &c) {
                        Ok(bytes) => Ok(journal::digest(&bytes) == expected),
                        Err(crate::papers::PaperError::Cancelled) => Err(IndexError::Cancelled),
                        Err(_) => Ok(false),
                    }
                })
                .await?
            } else {
                false
            };
            result.push((id, published, current));
        }
        Ok(result)
    }
    async fn journal<T, F>(&self, lane: Lane<'_>, f: F) -> IndexResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Journal, Cancellation) -> IndexResult<T> + Send + 'static,
    {
        let j = self.journal.clone();
        lane.run(move |c| joined(c, move |c| f(j, c))).await
    }
    async fn status(&self) -> IndexResult<IndexStatus> {
        let r = self.retrieval.clone();
        joined(Cancellation::default(), move |c| Ok(r.status(&c)?)).await
    }
    async fn fence(&self) -> IndexResult<Fence> {
        Ok(Fence::from_publication(
            &self
                .graph
                .observe_publication(Cancellation::default())
                .await?,
        ))
    }
    async fn source_matches(&self, version: &Version) -> IndexResult<bool> {
        let paper = version.validate()?.0.paper_id().to_owned();
        let expected = version.source.clone();
        let repo = self.papers.clone();
        joined(Cancellation::default(), move |c| {
            match repo.read_pdf_bytes(&paper, &c) {
                Ok(bytes) => Ok(journal::digest(&bytes) == expected),
                Err(
                    crate::papers::PaperError::NotFound
                    | crate::papers::PaperError::ChangedDuringRead,
                ) => Ok(false),
                Err(_) => Err(IndexError::Storage),
            }
        })
        .await
    }
    /// Provider-free startup/reopen recovery. Poison is cleared only after exact
    /// validation; any unwind/drop/error leaves it sticky before releasing write.
    pub async fn recover(&self) -> IndexResult<()> {
        let mut guard = self.gate.write(true).await?;
        if self
            .journal(Lane::Startup, |j, _| j.meta())
            .await?
            .is_none()
        {
            let status = self.status().await?;
            let fence = self.fence().await?;
            let graph = self.graph.status(Cancellation::default()).await?;
            if status.revision != 0
                || status.chunk_count != 0
                || status.exchange_count != 0
                || fence.generation != 0
                || graph.papers + graph.authors + graph.topics + graph.edges != 0
            {
                return Err(IndexError::RebuildRequired);
            }
            if status.lexical != LexicalStatus::Ready {
                let r = self.retrieval.clone();
                joined(Cancellation::default(), move |c| {
                    Ok(r.apply(&[PreparedOperation::RebuildLexical], Some(0), &c)?)
                })
                .await?;
            }
            let meta = Meta {
                fence,
                revision: 0,
                epoch: 0,
                identity: status.identity,
            };
            self.journal(Lane::Startup, move |j, c| j.bootstrap(&meta, &c))
                .await?;
        }
        if let Some(op) = self.journal(Lane::Startup, |j, _| j.operation()).await?
            && !op.phase.terminal()
        {
            self.resolve(op, Lane::Startup, false).await?;
        }
        self.verify_ready().await?;
        guard.ready();
        Ok(())
    }
    async fn verify_ready(&self) -> IndexResult<()> {
        let meta = self
            .journal(Lane::Startup, |j, _| j.meta())
            .await?
            .ok_or(IndexError::InvalidJournal)?;
        let status = self.status().await?;
        if meta.fence != self.fence().await?
            || status.revision != meta.revision
            || status.identity != meta.identity
            || status.lexical != LexicalStatus::Ready
        {
            return Err(IndexError::InvalidJournal);
        }
        if self
            .journal(Lane::Startup, |j, _| j.operation())
            .await?
            .is_some_and(|o| !o.phase.terminal())
        {
            return Err(IndexError::InvalidJournal);
        }
        let mut after = String::new();
        let mut papers = 0;
        let mut chunks = 0;
        loop {
            let cursor = after.clone();
            let next = self
                .journal(Lane::Startup, move |j, _| j.next_active(&cursor))
                .await?;
            let Some((id, v)) = next else { break };
            let (r, g) = v.validate()?;
            let store = self.retrieval.clone();
            let paper = id.clone();
            let observed = joined(Cancellation::default(), move |c| {
                Ok(store.observe_paper(&paper, &c)?)
            })
            .await?;
            if observed.artifact != r
                || observed.revision != meta.revision
                || observed.lexical != LexicalStatus::Ready
            {
                return Err(IndexError::InvalidJournal);
            }
            // Submitted labels in old versions are replay input, not active truth.
            let (fence, scope) = self
                .graph
                .publication_scope(id.clone(), g, Cancellation::default())
                .await?;
            if fence != meta.fence {
                return Err(IndexError::InvalidJournal);
            }
            self.journal(Lane::Startup, move |j, _| j.validate_scope(&scope))
                .await?;
            if r.has_rows() {
                papers += 1;
            }
            if let PreparedOperation::ReplacePaper(p) = r.restore_operation(&meta.identity)? {
                chunks += p.chunks().len();
            }
            after = id;
        }
        let graph = self.graph.status(Cancellation::default()).await?;
        if status.paper_count != papers || status.chunk_count != chunks || graph.papers != papers {
            return Err(IndexError::InvalidJournal);
        }
        self.journal(Lane::Startup, move |j, _| {
            j.validate_summary(graph.authors, graph.topics, graph.edges)
        })
        .await?;
        Ok(())
    }
    pub async fn published_paper(&self, paper: String) -> IndexResult<Option<PublishedPaper>> {
        let _guard = self.gate.read().await?;
        let result = async {
            self.verify_ready().await?;
            self.journal(Lane::Startup, move |j, _| {
                j.active(&paper)?
                    .as_ref()
                    .map(PublishedPaper::from_version)
                    .transpose()
            })
            .await
        }
        .await;
        if result.is_err() {
            self.gate.poison();
        }
        result
    }
    /// One already-prepared paper, not a production indexing handler. Every
    /// durable child belongs to the supplied cooperative request; nothing seals
    /// the entire job here. Caller joins its preparation group before completion.
    pub async fn publish(
        &self,
        prepared: PreparedPublication,
        group: &OperationGroup,
    ) -> IndexResult<PublishedPaper> {
        group.check()?;
        let mut guard = self.gate.write(false).await?;
        group.check()?;
        let (desired, graph) = prepared.version.validate()?;
        let id = desired.paper_id().to_owned();
        let lookup = id.clone();
        guard.arm();
        let prior = self
            .journal(Lane::Startup, move |j, _| j.active(&lookup))
            .await?;
        if prior.as_ref().map(|v| &v.id) != prepared.expected_version.as_ref()
            || !self.source_matches(&prepared.version).await?
        {
            guard.ready();
            return Err(IndexError::Conflict);
        }
        // Failure during authority checks is not a preparation failure: never
        // allow a possibly mixed store to remain Ready.
        guard.arm();
        let op = self
            .build_operation(prepared.version.clone(), prior, graph)
            .await;
        let op = match op {
            Ok(o) => o,
            Err(IndexError::Limit) => {
                guard.ready();
                return Err(IndexError::Limit);
            }
            Err(e) => return Err(e),
        };
        let to_save = op.clone();
        let result = async {
            self.journal(Lane::Request(group), move |j, c| j.prepare(&to_save, &c))
                .await?;
            self.resolve(op.clone(), Lane::Request(group), false).await
        }
        .await;
        if let Err(error) = result {
            // Stop the request even when a local read/validation (not a child)
            // failed. Compensation is only admitted after StopRequested.
            group.preparation_stop().cancel();
            let durable = self
                .journal(Lane::Compensation(group), |j, _| j.operation())
                .await?;
            if let Some(durable) = durable.filter(|o| o.id == op.id && !o.phase.terminal()) {
                self.resolve(durable, Lane::Compensation(group), true)
                    .await?;
            }
            self.verify_ready().await?;
            guard.ready();
            return Err(error);
        }
        self.verify_ready().await?;
        let durable = self.journal(Lane::Startup, |j, _| j.operation()).await?;
        guard.ready();
        if durable.is_none_or(|o| o.id != op.id || o.phase != Phase::Complete) {
            return Err(IndexError::Conflict);
        }
        PublishedPaper::from_version(&prepared.version)
    }
    async fn build_operation(
        &self,
        desired: Version,
        prior: Option<Version>,
        graph: GraphArtifact,
    ) -> IndexResult<Operation> {
        let meta = self
            .journal(Lane::Startup, |j, _| j.meta())
            .await?
            .ok_or(IndexError::InvalidJournal)?;
        let r = self.retrieval.clone();
        let id = graph.paper_id().to_owned();
        let observed = joined(Cancellation::default(), move |c| {
            Ok(r.observe_paper(&id, &c)?)
        })
        .await?;
        if observed.revision != meta.revision
            || observed.lexical != LexicalStatus::Ready
            || observed.artifact.identity() != &meta.identity
        {
            return Err(IndexError::InvalidJournal);
        }
        let (base, before) = self
            .graph
            .publication_scope(
                graph.paper_id().into(),
                graph.clone(),
                Cancellation::default(),
            )
            .await?;
        if base != meta.fence {
            return Err(IndexError::InvalidJournal);
        }
        let scope = before.clone();
        let affected = self
            .journal(Lane::Startup, move |j, _| j.validate_scope(&scope))
            .await?;
        let after = before.desired(&graph)?;
        let forward = base.next("forward", after.digest()?)?;
        let reverse = forward.next("compensate", before.digest()?)?;
        let op = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            base: meta,
            phase: Phase::Prepared,
            kind: Kind::Paper(Box::new(PaperOp {
                prior,
                desired,
                transition: Transition {
                    before,
                    desired: after,
                    base,
                    forward,
                    reverse,
                },
                affected,
            })),
        };
        let Kind::Paper(p) = &op.kind else {
            unreachable!()
        };
        if observed.artifact != p.before_retrieval()? {
            return Err(IndexError::InvalidJournal);
        }
        op.validate()?;
        op.check_size()?;
        Ok(op)
    }
    async fn resolve(&self, op: Operation, lane: Lane<'_>, rollback: bool) -> IndexResult<()> {
        let check = op.clone();
        self.journal(lane, move |j, _| j.validate_pending(&check))
            .await?;
        match &op.kind {
            Kind::Reset { .. } => self.resolve_reset(op, lane).await,
            Kind::Paper(p) => {
                let (desired, _) = p.desired.validate()?;
                let prior = p.before_retrieval()?;
                let (fence, scope) = self
                    .graph
                    .exact_scope(p.transition.before.clone(), Cancellation::default())
                    .await?;
                let at_before = fence == p.transition.base && scope == p.transition.before;
                let at_forward = fence == p.transition.forward && scope == p.transition.desired;
                let at_reverse = fence == p.transition.reverse && scope == p.transition.before;
                if !at_before && !at_forward && !at_reverse {
                    return Err(IndexError::InvalidJournal);
                }
                let restore = rollback
                    || op.phase == Phase::Compensating
                    || at_reverse
                    || !self.source_matches(&p.desired).await?;
                let r = self.retrieval.clone();
                let id = desired.paper_id().to_owned();
                let current = joined(Cancellation::default(), move |c| {
                    Ok(r.observe_paper(&id, &c)?)
                })
                .await?;
                let forward_revision = op
                    .base
                    .revision
                    .checked_add(i64::from(prior != desired || !desired.has_rows()))
                    .ok_or(IndexError::InvalidJournal)?;
                let reverse_revision = forward_revision
                    .checked_add(i64::from(prior != desired || !prior.has_rows()))
                    .ok_or(IndexError::InvalidJournal)?;
                let retrieval_before =
                    current.artifact == prior && current.revision == op.base.revision;
                let retrieval_forward =
                    current.artifact == desired && current.revision == forward_revision;
                let retrieval_reverse = current.artifact == prior
                    && current.revision == reverse_revision
                    && (at_reverse || op.phase == Phase::Compensating);
                if current.lexical != LexicalStatus::Ready
                    || !(retrieval_before || retrieval_forward || retrieval_reverse)
                    || (at_forward && !retrieval_forward && !retrieval_reverse)
                {
                    return Err(IndexError::InvalidJournal);
                }
                if restore {
                    let id = op.id.clone();
                    self.journal(lane, move |j, c| j.phase(&id, Phase::Compensating, &c))
                        .await?;
                    if at_forward {
                        let graph = self.graph.clone();
                        let t = p.transition.clone();
                        lane.run(move |c| async move { Ok(graph.transition(t, true, c).await?) })
                            .await?;
                    }
                    if !retrieval_before && !retrieval_reverse {
                        self.restore(prior, lane, current.revision).await?;
                    }
                    let fence = if at_before {
                        p.transition.base.clone()
                    } else {
                        p.transition.reverse.clone()
                    };
                    self.finish_exact(&op, false, fence, lane).await
                } else {
                    if !retrieval_forward {
                        self.restore(desired, lane, current.revision).await?;
                    }
                    if op.phase == Phase::Prepared {
                        let id = op.id.clone();
                        self.journal(lane, move |j, c| j.phase(&id, Phase::RetrievalApplied, &c))
                            .await?;
                    }
                    if at_before {
                        let graph = self.graph.clone();
                        let t = p.transition.clone();
                        lane.run(move |c| async move { Ok(graph.transition(t, false, c).await?) })
                            .await?;
                    }
                    let id = op.id.clone();
                    let phase = self
                        .journal(lane, |j, _| {
                            Ok(j.operation()?.ok_or(IndexError::InvalidJournal)?.phase)
                        })
                        .await?;
                    if phase == Phase::RetrievalApplied {
                        self.journal(lane, move |j, c| j.phase(&id, Phase::GraphApplied, &c))
                            .await?;
                    }
                    self.finish_exact(&op, true, p.transition.forward.clone(), lane)
                        .await
                }
            }
        }
    }
    async fn restore(
        &self,
        artifact: RetrievalArtifact,
        lane: Lane<'_>,
        revision: i64,
    ) -> IndexResult<()> {
        let r = self.retrieval.clone();
        lane.run(move |c| {
            joined(c, move |c| {
                r.apply(
                    &[
                        artifact.restore_operation(r.identity())?,
                        PreparedOperation::RebuildLexical,
                    ],
                    Some(revision),
                    &c,
                )?;
                Ok(())
            })
        })
        .await
    }
    async fn finish_exact(
        &self,
        op: &Operation,
        publish: bool,
        fence: Fence,
        lane: Lane<'_>,
    ) -> IndexResult<()> {
        if let Kind::Paper(p) = &op.kind {
            let expected = if publish {
                &p.transition.desired
            } else {
                &p.transition.before
            };
            let (f, s) = self
                .graph
                .exact_scope(expected.clone(), Cancellation::default())
                .await?;
            let expected_r = if publish {
                p.desired.validate()?.0
            } else {
                p.before_retrieval()?
            };
            let r = self.retrieval.clone();
            let id = expected_r.paper_id().to_owned();
            let current = joined(Cancellation::default(), move |c| {
                Ok(r.observe_paper(&id, &c)?)
            })
            .await?;
            if f != fence
                || &s != expected
                || current.artifact != expected_r
                || current.lexical != LexicalStatus::Ready
            {
                return Err(IndexError::InvalidJournal);
            }
        }
        let revision = self.status().await?.revision;
        let id = op.id.clone();
        self.journal(lane, move |j, c| {
            j.finish(&id, publish, fence, revision, &c)
        })
        .await
    }
    /// Only an explicitly requested derived reset may call this. There is no
    /// automatic reset on open/recovery mismatch, and no PDF/history handle here.
    pub async fn reset(&self, group: &OperationGroup) -> IndexResult<()> {
        let mut guard = self.gate.write(false).await?;
        group.check()?;
        guard.arm();
        self.verify_ready().await?;
        let base = self
            .journal(Lane::Startup, |j, _| j.meta())
            .await?
            .ok_or(IndexError::InvalidJournal)?;
        let next = base.fence.next(
            "coordinated_reset",
            journal::digest(b"empty-derived-index-v1"),
        )?;
        let op = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            base,
            phase: Phase::ResetPrepared,
            kind: Kind::Reset { next },
        };
        let save = op.clone();
        if let Err(error) = self
            .journal(Lane::Request(group), move |j, c| j.prepare(&save, &c))
            .await
        {
            group.preparation_stop().cancel();
            let durable = self
                .journal(Lane::Compensation(group), |j, _| j.operation())
                .await?;
            if let Some(durable) = durable.filter(|o| o.id == op.id && !o.phase.terminal()) {
                self.resolve_reset(durable, Lane::Compensation(group))
                    .await?;
            }
            self.verify_ready().await?;
            guard.ready();
            return Err(error);
        }
        let finalization = match group.begin_irreversible_plan(op.id.clone()) {
            Ok(f) => f,
            Err(e) => {
                self.resolve_reset(op, Lane::Compensation(group)).await?;
                self.verify_ready().await?;
                guard.ready();
                return Err(e.into());
            }
        };
        let id = op.id.clone();
        self.journal(
            Lane::Reset(&finalization, ResetStep::PersistFinalizing),
            move |j, c| j.phase(&id, Phase::ResetFinalizing, &c),
        )
        .await?;
        let r = self.retrieval.clone();
        let revision = op.base.revision;
        Lane::Reset(&finalization, ResetStep::ResetRetrieval)
            .run(move |c| {
                joined(c, move |c| {
                    Ok(r.apply(
                        &[
                            PreparedOperation::Reset(PreparedReset::new(r.identity().clone())?),
                            PreparedOperation::RebuildLexical,
                        ],
                        Some(revision),
                        &c,
                    )?)
                })
            })
            .await?;
        let id = op.id.clone();
        self.journal(
            Lane::Reset(&finalization, ResetStep::RecordRetrieval),
            move |j, c| j.phase(&id, Phase::ResetRetrieval, &c),
        )
        .await?;
        let Kind::Reset { next } = &op.kind else {
            unreachable!()
        };
        let graph = self.graph.clone();
        let base = op.base.fence.clone();
        let next = next.clone();
        Lane::Reset(&finalization, ResetStep::ResetGraph)
            .run(move |c| async move { Ok(graph.coordinated_reset(base, next, c).await?) })
            .await?;
        let id = op.id.clone();
        self.journal(
            Lane::Reset(&finalization, ResetStep::RecordGraph),
            move |j, c| j.phase(&id, Phase::ResetGraph, &c),
        )
        .await?;
        self.complete_reset(&op, Lane::Reset(&finalization, ResetStep::CompleteJournal))
            .await?;
        self.verify_ready().await?;
        guard.ready();
        let _seal = finalization.seal_completion()?;
        Ok(())
    }
    async fn resolve_reset(&self, op: Operation, lane: Lane<'_>) -> IndexResult<()> {
        let Kind::Reset { next } = &op.kind else {
            return Err(IndexError::InvalidJournal);
        };
        let fence = self.fence().await?;
        let status = self.status().await?;
        if op.phase == Phase::ResetPrepared {
            if fence != op.base.fence || status.revision != op.base.revision {
                return Err(IndexError::InvalidJournal);
            }
            let id = op.id;
            return self
                .journal(lane, move |j, c| {
                    j.finish(&id, false, fence, status.revision, &c)
                })
                .await;
        }
        let revision = op
            .base
            .revision
            .checked_add(1)
            .ok_or(IndexError::InvalidJournal)?;
        if status.revision == op.base.revision && fence == op.base.fence {
            let r = self.retrieval.clone();
            lane.run(move |c| {
                joined(c, move |c| {
                    Ok(r.apply(
                        &[
                            PreparedOperation::Reset(PreparedReset::new(r.identity().clone())?),
                            PreparedOperation::RebuildLexical,
                        ],
                        Some(status.revision),
                        &c,
                    )?)
                })
            })
            .await?;
        } else if status.revision != revision
            || status.chunk_count != 0
            || status.exchange_count != 0
            || status.lexical != LexicalStatus::Ready
        {
            return Err(IndexError::InvalidJournal);
        }
        if fence == op.base.fence {
            let graph = self.graph.clone();
            let base = fence;
            let next = next.clone();
            lane.run(move |c| async move { Ok(graph.coordinated_reset(base, next, c).await?) })
                .await?;
        } else if &fence != next {
            return Err(IndexError::InvalidJournal);
        }
        self.complete_reset(&op, lane).await
    }
    async fn complete_reset(&self, op: &Operation, lane: Lane<'_>) -> IndexResult<()> {
        let Kind::Reset { next } = &op.kind else {
            return Err(IndexError::InvalidJournal);
        };
        let status = self.status().await?;
        let graph = self.graph.status(Cancellation::default()).await?;
        if self.fence().await? != *next
            || graph.papers + graph.authors + graph.topics + graph.edges != 0
            || status.chunk_count + status.exchange_count != 0
            || status.lexical != LexicalStatus::Ready
            || status.revision
                != op
                    .base
                    .revision
                    .checked_add(1)
                    .ok_or(IndexError::InvalidJournal)?
        {
            return Err(IndexError::InvalidJournal);
        }
        let id = op.id.clone();
        let next = next.clone();
        self.journal(lane, move |j, c| {
            j.finish(&id, true, next, status.revision, &c)
        })
        .await
    }
}
