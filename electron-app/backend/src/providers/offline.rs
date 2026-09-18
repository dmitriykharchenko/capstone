use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    domain::Message,
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};

use super::{
    EmbeddingClient, ModelClient, ProviderError, ProviderResult,
    embedding::validate_texts,
    model::{validate_answer, validate_messages},
    transport::check,
};

/// Explicit offline selection only; HTTP failures never silently fall back here.
#[derive(Clone, Default)]
pub struct MockModelClient;

impl MockModelClient {
    fn reply(messages: &[Message]) -> ProviderResult<String> {
        validate_messages(messages)?;
        let user = &messages.last().ok_or(ProviderError::InvalidInput)?.content;
        let marker = |name: &str| {
            messages
                .iter()
                .any(|m| m.content.lines().next() == Some(name))
        };
        let result = if marker("MULTIHOP_PLANNER") {
            json!({"subqueries":[format!("Evidence supporting {}",user.trim()),
                format!("Evidence limiting or contrasting {}",user.trim())]})
            .to_string()
        } else if marker("MULTIHOP_SUBANSWER") {
            format!("[offline mock evidence summary] {}", user.trim())
        } else if marker("MULTIHOP_COVERAGE") {
            json!({"complete":true,"follow_up_subqueries":[]}).to_string()
        } else if marker("MULTIHOP_COMPOSER") {
            let question = serde_json::from_str::<Value>(user)
                .ok()
                .and_then(|v| {
                    v.get("original_question")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| user.clone());
            format!("[offline mock reply] {}", question.trim())
        } else if user.starts_with("METADATA_EXTRACTION_INPUT") {
            let paper = user
                .split_once("<paper_text>\n")
                .map(|(_, text)| text)
                .unwrap_or("")
                .split("\n</paper_text>")
                .next()
                .unwrap_or("");
            let title: String = paper
                .lines()
                .map(str::trim)
                .find(|s| !s.is_empty())
                .unwrap_or("Untitled paper")
                .chars()
                .take(200)
                .collect();
            json!({"title":title,"authors":["Unknown author"],"topics":["research","paper","local corpus"]}).to_string()
        } else {
            format!(
                "[offline mock reply] No API key is configured, so this is a deterministic response. You said: {}",
                user.trim()
            )
        };
        validate_answer(&result)?;
        Ok(result)
    }
}

impl ModelClient for MockModelClient {
    fn complete(
        &self,
        messages: Vec<Message>,
        cancellation: Cancellation,
    ) -> BackendFuture<String> {
        Box::pin(async move {
            check(&cancellation)?;
            let reply = Self::reply(&messages)?;
            check(&cancellation)?;
            Ok(reply)
        })
    }

    fn stream(&self, messages: Vec<Message>, context: RequestContext) -> BackendFuture<String> {
        Box::pin(async move {
            let reply = Self::reply(&messages)?;
            let mut start = 0;
            let mut whitespace = false;
            for (offset, character) in reply.char_indices() {
                if whitespace && !character.is_whitespace() {
                    context
                        .emit("delta", json!({"delta":&reply[start..offset]}))
                        .await?;
                    start = offset;
                }
                whitespace = character.is_whitespace();
            }
            context
                .emit("delta", json!({"delta":&reply[start..]}))
                .await?;
            Ok(reply)
        })
    }
}

/// Legacy SHA-256 signed token hashing (256 dimensions by default). Rebuilt
/// indexes must use the same client/dimension for both documents and queries.
#[derive(Clone)]
pub struct LocalEmbeddingClient {
    dimensions: usize,
}

impl Default for LocalEmbeddingClient {
    fn default() -> Self {
        Self { dimensions: 256 }
    }
}

impl LocalEmbeddingClient {
    pub fn new(dimensions: usize) -> ProviderResult<Self> {
        if dimensions == 0 || dimensions > 65_536 {
            return Err(ProviderError::Configuration);
        }
        Ok(Self { dimensions })
    }

    fn vector(&self, text: &str, cancellation: &Cancellation) -> ProviderResult<Vec<f64>> {
        let mut vector = vec![0.0_f64; self.dimensions];
        for token in text
            .to_lowercase()
            .split(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
            .filter(|s| !s.is_empty())
        {
            check(cancellation)?;
            let digest = Sha256::digest(token.as_bytes());
            let bucket = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) as usize
                % self.dimensions;
            vector[bucket] += if digest[4] & 1 != 0 { 1.0 } else { -1.0 };
        }
        let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
        if norm == 0.0 {
            vector[0] = 1.0;
        } else {
            vector.iter_mut().for_each(|v| *v /= norm);
        }
        Ok(vector)
    }
}

impl EmbeddingClient for LocalEmbeddingClient {
    fn embed(
        &self,
        texts: Vec<String>,
        cancellation: Cancellation,
    ) -> BackendFuture<Vec<Vec<f64>>> {
        let client = self.clone();
        Box::pin(async move {
            check(&cancellation)?;
            validate_texts(&texts)?;
            // CPU work remains cooperative if the awaiting future is dropped.
            tokio::task::spawn_blocking(move || {
                let result: ProviderResult<Vec<_>> = texts
                    .iter()
                    .map(|text| client.vector(text, &cancellation))
                    .collect();
                check(&cancellation)?;
                result
            })
            .await
            .map_err(|_| crate::protocol::ErrorCode::Internal)?
            .map_err(Into::into)
        })
    }
}
