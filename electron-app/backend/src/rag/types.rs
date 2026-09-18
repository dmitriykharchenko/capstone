use crate::{
    domain::{Chunk, RagResult},
    protocol::ErrorCode,
    providers::ProviderError,
    runtime::{Cancellation, RequestContext},
};

#[derive(Clone)]
pub enum RunMode {
    /// No RAG events or model streams, even when nested in evaluate_stream.
    Silent(Cancellation),
    /// Public progress and final answer deltas; never emits a nested terminal.
    Public(RequestContext),
}

impl RunMode {
    pub(super) fn cancellation(&self) -> Cancellation {
        match self {
            Self::Silent(token) => token.clone(),
            Self::Public(context) => context.cancellation(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RagStage {
    Input,
    Planner,
    Retrieval,
    Subanswer,
    Coverage,
    FollowupRetrieval,
    FollowupSubanswer,
    Composer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RagErrorKind {
    InvalidInput,
    InvalidPlan,
    InvalidCoverage,
    InvalidEvidence,
    EmptyResponse,
    TooLarge,
    Cancelled,
    Provider(ProviderError),
    /// The foundation ModelClient/Retriever interfaces erase richer distinctions.
    Backend(ErrorCode),
}

/// Sanitized stage/category only: never contains provider text, prompts or paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RagError {
    pub stage: RagStage,
    pub kind: RagErrorKind,
}

impl RagError {
    pub fn new(stage: RagStage, kind: RagErrorKind) -> Self {
        Self { stage, kind }
    }

    pub(super) fn backend(stage: RagStage, code: ErrorCode) -> Self {
        Self::new(
            stage,
            match code {
                ErrorCode::Cancelled => RagErrorKind::Cancelled,
                ErrorCode::ResponseTooLarge => RagErrorKind::TooLarge,
                _ => RagErrorKind::Backend(code),
            },
        )
    }
}

impl std::fmt::Display for RagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RAG {:?} failed: {:?}.", self.stage, self.kind)
    }
}

impl std::error::Error for RagError {}

impl From<RagError> for ErrorCode {
    fn from(error: RagError) -> Self {
        match error.kind {
            RagErrorKind::Cancelled => Self::Cancelled,
            RagErrorKind::InvalidInput => Self::InvalidParams,
            RagErrorKind::TooLarge => Self::ResponseTooLarge,
            RagErrorKind::Provider(error) => error.into(),
            RagErrorKind::Backend(code) => code,
            _ => Self::Internal,
        }
    }
}

/// Internal evidence/provenance trace for evaluations, NOT a public event payload.
#[derive(Debug)]
pub struct HopEvidence {
    pub subquery: String,
    pub answer: String,
    pub context: String,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug)]
pub struct RagRun {
    pub result: RagResult,
    pub initial_count: usize,
    pub followup_subqueries: Vec<String>,
    pub hops: Vec<HopEvidence>,
}
