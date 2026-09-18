use std::{future::Future, pin::Pin, sync::Arc};

use crate::{
    domain::Message,
    protocol::ErrorCode,
    providers::{HttpModelClient, ModelClient, ProviderError},
    runtime::{Cancellation, RequestContext},
};

use super::{RagError, RagErrorKind, RagStage};

pub type ModelFuture = Pin<Box<dyn Future<Output = Result<String, ModelFailure>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFailure {
    Provider(ProviderError),
    Backend(ErrorCode),
}

impl ModelFailure {
    pub(super) fn at(self, stage: RagStage) -> RagError {
        match self {
            Self::Provider(ProviderError::Cancelled) => {
                RagError::new(stage, RagErrorKind::Cancelled)
            }
            Self::Provider(error) => RagError::new(stage, RagErrorKind::Provider(error)),
            Self::Backend(code) => RagError::backend(stage, code),
        }
    }
}

/// Local typed adapter contract. Private completion MUST NOT publish anything.
/// Streaming MUST emit only final `delta` events, return their authoritative text,
/// and fail on truncation/transport loss/refusal (never return a partial success).
/// Implementations must stop on cancellation/drop; no detached work or persistence.
pub trait RagModel: Send + Sync {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation) -> ModelFuture;
    fn stream_final(&self, messages: Vec<Message>, context: RequestContext) -> ModelFuture;
}

/// Preferred production adapter: retains IncompleteResponse vs other failures.
impl RagModel for HttpModelClient {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation) -> ModelFuture {
        let model = self.clone();
        Box::pin(async move {
            model
                .complete_response(messages, None, cancellation)
                .await
                .map_err(ModelFailure::Provider)
        })
    }

    fn stream_final(&self, messages: Vec<Message>, context: RequestContext) -> ModelFuture {
        let model = self.clone();
        Box::pin(async move {
            HttpModelClient::stream_final(&model, messages, context)
                .await
                .map_err(ModelFailure::Provider)
        })
    }
}

pub(super) struct FoundationModel(pub Arc<dyn ModelClient>);

impl RagModel for FoundationModel {
    fn complete(&self, messages: Vec<Message>, cancellation: Cancellation) -> ModelFuture {
        let future = self.0.complete(messages, cancellation);
        Box::pin(async move { future.await.map_err(ModelFailure::Backend) })
    }

    fn stream_final(&self, messages: Vec<Message>, context: RequestContext) -> ModelFuture {
        let future = self.0.stream(messages, context);
        Box::pin(async move { future.await.map_err(ModelFailure::Backend) })
    }
}
