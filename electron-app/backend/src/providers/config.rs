use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::{ProviderError, ProviderResult};

pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ANSWER_BYTES: usize = 512 * 1024;
pub(crate) const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Limits apply to the entire operation, including all retries and stream reads.
#[derive(Clone, Debug)]
pub struct RequestPolicy {
    pub timeout: Duration,
    pub max_retries: u8,
    pub max_body_bytes: usize,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_retries: 2,
            max_body_bytes: MAX_BODY_BYTES,
        }
    }
}

impl RequestPolicy {
    pub(crate) fn validate(&self) -> ProviderResult<()> {
        if self.timeout.is_zero()
            || self.timeout > Duration::from_secs(600)
            || self.max_retries > 2
            || self.max_body_bytes == 0
            || self.max_body_bytes > MAX_BODY_BYTES
        {
            return Err(ProviderError::Configuration);
        }
        Ok(())
    }
}

/// Not serializable or Debug: credentials and endpoint configuration stay backend-only.
#[derive(Clone)]
pub struct ModelConfig {
    pub model: String,
    pub base_url: String,
    pub(crate) api_key: String,
    pub temperature: f64,
    pub max_tokens: Option<u32>,
    pub policy: RequestPolicy,
}

impl ModelConfig {
    pub fn new(model: String, base_url: String, api_key: String) -> Self {
        Self {
            model,
            base_url,
            api_key,
            temperature: 0.2,
            max_tokens: None,
            policy: RequestPolicy::default(),
        }
    }
}

#[derive(Clone)]
pub struct EmbeddingConfig {
    pub model: String,
    pub base_url: String,
    pub(crate) api_key: String,
    pub expected_dimensions: Option<usize>,
    pub policy: RequestPolicy,
}

impl EmbeddingConfig {
    pub fn new(model: String, base_url: String, api_key: String) -> Self {
        Self {
            model,
            base_url,
            api_key,
            expected_dimensions: None,
            policy: RequestPolicy::default(),
        }
    }
}

/// JSON schema is supplied by backend domain code, never by renderer parameters.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema { json_schema: JsonSchema },
}

#[derive(Clone, Debug, Serialize)]
pub struct JsonSchema {
    name: String,
    schema: Value,
    strict: bool,
}

impl ResponseFormat {
    pub fn json_schema(name: String, schema: Value) -> ProviderResult<Self> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !schema.is_object()
        {
            return Err(ProviderError::InvalidInput);
        }
        Ok(Self::JsonSchema {
            json_schema: JsonSchema {
                name,
                schema,
                strict: true,
            },
        })
    }
}
