use crate::{domain::EvaluationCase, registry::BackendFuture, runtime::RequestContext};
use serde_json::Value;

pub trait EvaluationService: Send + Sync {
    // Shared RagPipeline; no Chat persistence. Return only after a terminal
    // artifact is atomically persisted in the new-app writable data directory.
    fn run(&self, cases: Vec<EvaluationCase>, context: RequestContext) -> BackendFuture<Value>;
}
