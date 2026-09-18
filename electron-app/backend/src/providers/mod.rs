//! Native providers. Use `complete`/`complete_json` for private planner, judge,
//! coverage and metadata stages. `stream` is ONLY for the final public answer;
//! it emits content deltas, never provider reasoning or intermediate messages.
mod config;
mod embedding;
mod model;
mod offline;
mod sse;
mod transport;

pub use config::{EmbeddingConfig, ModelConfig, RequestPolicy, ResponseFormat};
pub use embedding::HttpEmbeddingClient;
pub use model::HttpModelClient;
pub use offline::{LocalEmbeddingClient, MockModelClient};

use crate::{
    domain::Message,
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};

pub trait ModelClient: Send + Sync {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation)
    -> BackendFuture<String>;
    fn stream(&self, messages: Vec<Message>, context: RequestContext) -> BackendFuture<String>;
}

pub trait EmbeddingClient: Send + Sync {
    fn embed(&self, texts: Vec<String>, cancellation: Cancellation)
    -> BackendFuture<Vec<Vec<f64>>>;
}

/// Deliberately contains no upstream body, URL, credential, prompt or error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderError {
    Configuration,
    InvalidInput,
    Authentication,
    RateLimited,
    Http,
    Transport,
    Timeout,
    Cancelled,
    TooLarge,
    MalformedResponse,
    IncompleteResponse,
    Refused,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Invalid provider configuration.",
            Self::InvalidInput => "Invalid provider input.",
            Self::Authentication => "Provider authentication failed.",
            Self::RateLimited => "Provider rate limit reached.",
            Self::Http => "Provider request failed.",
            Self::Transport => "Provider connection failed.",
            Self::Timeout => "Provider request timed out.",
            Self::Cancelled => "Provider request cancelled.",
            Self::TooLarge => "Provider payload exceeds the byte limit.",
            Self::MalformedResponse => "Provider returned an invalid response.",
            Self::IncompleteResponse => "Provider response was incomplete.",
            Self::Refused => "Provider declined to complete the response.",
        })
    }
}

impl std::error::Error for ProviderError {}

impl From<ProviderError> for crate::protocol::ErrorCode {
    fn from(error: ProviderError) -> Self {
        use crate::protocol::ErrorCode;
        match error {
            ProviderError::Cancelled => ErrorCode::Cancelled,
            ProviderError::InvalidInput | ProviderError::Configuration => ErrorCode::InvalidParams,
            ProviderError::TooLarge => ErrorCode::ResponseTooLarge,
            _ => ErrorCode::Internal,
        }
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;
