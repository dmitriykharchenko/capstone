use serde::Deserialize;
use serde_json::json;

use crate::{registry::BackendFuture, runtime::Cancellation};

use super::{
    EmbeddingClient, EmbeddingConfig, ProviderError, ProviderResult,
    config::MAX_BODY_BYTES,
    transport::{Transport, check, guarded},
};

#[derive(Clone)]
pub struct HttpEmbeddingClient {
    config: EmbeddingConfig,
    transport: Transport,
}

impl HttpEmbeddingClient {
    pub fn new(config: EmbeddingConfig) -> ProviderResult<Self> {
        if config.model.trim().is_empty()
            || config.expected_dimensions == Some(0)
            || config.expected_dimensions.is_some_and(|d| d > 65_536)
        {
            return Err(ProviderError::Configuration);
        }
        let transport = Transport::new(
            &config.base_url,
            "embeddings",
            &config.api_key,
            config.policy.clone(),
        )?;
        Ok(Self { config, transport })
    }

    pub async fn embed_batch(
        &self,
        texts: Vec<String>,
        cancellation: Cancellation,
    ) -> ProviderResult<Vec<Vec<f64>>> {
        check(&cancellation)?;
        validate_texts(&texts)?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let count = texts.len();
        let body = json!({"model":self.config.model,"input":texts,"encoding_format":"float"});
        guarded(&cancellation, self.config.policy.timeout, async {
            let value = self.transport.json(&body).await?;
            if value.get("error").is_some_and(|v| !v.is_null()) {
                return Err(ProviderError::Http);
            }
            let response: Embeddings =
                serde_json::from_value(value).map_err(|_| ProviderError::MalformedResponse)?;
            if response.data.len() != count {
                return Err(ProviderError::MalformedResponse);
            }
            let mut ordered = vec![None; count];
            let mut dimensions = self.config.expected_dimensions;
            for item in response.data {
                if item.index >= count
                    || ordered[item.index].is_some()
                    || item.embedding.is_empty()
                    || item.embedding.iter().any(|v| !v.is_finite())
                {
                    return Err(ProviderError::MalformedResponse);
                }
                let dimension = *dimensions.get_or_insert(item.embedding.len());
                if dimension != item.embedding.len() {
                    return Err(ProviderError::MalformedResponse);
                }
                ordered[item.index] = Some(item.embedding);
            }
            ordered
                .into_iter()
                .map(|v| v.ok_or(ProviderError::MalformedResponse))
                .collect()
        })
        .await
    }
}

impl EmbeddingClient for HttpEmbeddingClient {
    fn embed(
        &self,
        texts: Vec<String>,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Vec<f64>>> {
        let client = self.clone();
        Box::pin(async move {
            client
                .embed_batch(texts, cancellation)
                .await
                .map_err(Into::into)
        })
    }
}

pub(crate) fn validate_texts(texts: &[String]) -> ProviderResult<()> {
    if texts.iter().any(|s| s.trim().is_empty()) {
        return Err(ProviderError::InvalidInput);
    }
    if texts.len() > 2048
        || texts
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()))
            .is_none_or(|n| n > MAX_BODY_BYTES)
    {
        return Err(ProviderError::TooLarge);
    }
    Ok(())
}

#[derive(Deserialize)]
struct Embeddings {
    data: Vec<Embedding>,
}

#[derive(Deserialize)]
struct Embedding {
    index: usize,
    // Serde rejects booleans, strings, nulls and nonfinite/out-of-range JSON numbers.
    embedding: Vec<f64>,
}
