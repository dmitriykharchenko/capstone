//! Stateless adaptive RAG shared by Chat and evaluations. No history/index writes,
//! command registration, environment discovery, or runtime legacy resources.
//! Use `RunMode::Silent` for evaluations even inside a streaming evaluation request.
//! Only final composition streams; the outer runtime alone publishes terminals.
//! Deltas are provisional: use the returned result only after successful completion.
mod evidence;
mod model;
mod normalization;
mod pipeline;
mod prompts;
mod types;
mod validation;

pub use model::{ModelFailure, ModelFuture, RagModel};
pub use pipeline::AdaptiveRag;
pub use types::{HopEvidence, RagError, RagErrorKind, RagRun, RagStage, RunMode};

pub const PAPER_CHUNK_LIMIT: usize = 5;
pub const MIN_SUBQUERY_COUNT: usize = 2;
pub const MAX_SUBQUERY_COUNT: usize = 5;
pub const MAX_FOLLOWUP_COUNT: usize = 2;
pub const MAX_TOTAL_HOPS: usize = 7;
pub const MAX_SOURCES: usize = 35;
pub const SOURCE_SNIPPET_LENGTH: usize = 240;

use crate::{
    domain::{Message, RagResult},
    registry::BackendFuture,
    runtime::RequestContext,
};

pub trait RagPipeline: Send + Sync {
    // Stateless shared Chat/Evals path. Public progress only; initial 2–5 hops,
    // one coverage review, <=2 followups, <=7 hops and <=35 aggregate sources.
    fn run(
        &self,
        question: String,
        history: Vec<Message>,
        context: RequestContext,
    ) -> BackendFuture<RagResult>;
}
