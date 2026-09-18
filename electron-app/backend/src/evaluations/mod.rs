//! Stateless evaluations. Production command registration is intentionally absent.
//! Native filesystem calls run inline on the runtime's blocking-capable request
//! runner; no detached workers, history access, or environment discovery.
mod contracts;
mod judge;
mod runner;
mod storage;
mod strict_json;

pub use contracts::{
    AggregateScores, CaseError, CaseResult, Complete, EvaluationArtifact, EvaluationRequest,
    RetrievedChunk, load_default_cases, parse_test_cases, validate_test_cases,
};
pub use judge::{JUDGE_CRITERIA, JudgeScores, JudgeTemplate, parse_judge_output};
pub use runner::NativeEvaluationService;
pub use storage::{ArtifactStore, RESULTS_FILENAME};

use crate::{domain::EvaluationCase, registry::BackendFuture, runtime::RequestContext};
use serde_json::Value;

pub trait EvaluationService: Send + Sync {
    // Shared RagPipeline; no Chat persistence. Return only after a terminal
    // artifact is atomically persisted in the new-app writable data directory.
    fn run(&self, cases: Vec<EvaluationCase>, context: RequestContext) -> BackendFuture<Value>;
}
