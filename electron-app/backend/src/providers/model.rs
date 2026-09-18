use futures_util::StreamExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    domain::Message,
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};

use super::{
    ModelClient, ModelConfig, ProviderError, ProviderResult, ResponseFormat,
    config::MAX_ANSWER_BYTES,
    sse::Decoder,
    transport::{Transport, guarded},
};

#[derive(Clone)]
pub struct HttpModelClient {
    config: ModelConfig,
    transport: Transport,
}

impl HttpModelClient {
    pub fn new(config: ModelConfig) -> ProviderResult<Self> {
        if config.model.trim().is_empty()
            || !config.temperature.is_finite()
            || config.temperature < 0.0
            || config.max_tokens == Some(0)
        {
            return Err(ProviderError::Configuration);
        }
        let transport = Transport::new(
            &config.base_url,
            "chat/completions",
            &config.api_key,
            config.policy.clone(),
        )?;
        Ok(Self { config, transport })
    }

    fn body(
        &self,
        messages: &[Message],
        stream: bool,
        format: Option<ResponseFormat>,
    ) -> ProviderResult<Value> {
        validate_messages(messages)?;
        // Do not forward storage IDs/source snapshots as chat-completion fields.
        let messages: Vec<_> = messages
            .iter()
            .map(|m| json!({"role":m.role,"content":m.content}))
            .collect();
        let mut body = json!({"model":self.config.model,"messages":messages,
            "temperature":self.config.temperature,"stream":stream});
        if let Some(limit) = self.config.max_tokens {
            body["max_tokens"] = json!(limit);
        }
        if let Some(format) = format {
            body["response_format"] = json!(format);
        }
        Ok(body)
    }

    /// Nonstreaming, with no public events. Domain callers must validate their DTO
    /// after parsing (planner hop limits, judge scores, metadata nonempty fields).
    pub async fn complete_response(
        &self,
        messages: Vec<Message>,
        format: Option<ResponseFormat>,
        cancellation: Cancellation,
    ) -> ProviderResult<String> {
        let body = self.body(&messages, false, format)?;
        guarded(&cancellation, self.config.policy.timeout, async {
            let value = self.transport.json(&body).await?;
            check_response(&value)?;
            let response: Completion =
                serde_json::from_value(value).map_err(|_| ProviderError::MalformedResponse)?;
            let choice = only_choice(response.choices)?;
            finish(choice.finish_reason.as_deref())?;
            let message = choice.message.ok_or(ProviderError::MalformedResponse)?;
            message.validate()?;
            let text = message
                .content
                .ok_or(ProviderError::MalformedResponse)?
                .into_text()?;
            validate_answer(&text)?;
            Ok(text)
        })
        .await
    }

    /// Requests JSON and deserializes directly into a backend-owned typed DTO.
    /// A schema request does not replace domain validation by the caller.
    pub async fn complete_json<T: DeserializeOwned>(
        &self,
        messages: Vec<Message>,
        format: ResponseFormat,
        cancellation: Cancellation,
    ) -> ProviderResult<T> {
        let text = self
            .complete_response(messages, Some(format), cancellation)
            .await?;
        serde_json::from_str(&text).map_err(|_| ProviderError::MalformedResponse)
    }

    /// Final composition only. Partial deltas are provisional until this returns
    /// success; callers must never persist them on error/cancellation/truncation.
    pub async fn stream_final(
        &self,
        messages: Vec<Message>,
        context: RequestContext,
    ) -> ProviderResult<String> {
        let body = self.body(&messages, true, None)?;
        let cancellation = context.cancellation();
        guarded(&cancellation, self.config.policy.timeout, async {
            let response = self.transport.post(&body, true).await?;
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !content_type
                .split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
            {
                return Err(ProviderError::MalformedResponse);
            }
            let mut bytes = response.bytes_stream();
            let mut decoder = Decoder::default();
            let mut total = 0usize;
            let mut text = String::new();
            let mut stopped = false;
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|error| {
                    if error.is_timeout() {
                        ProviderError::Timeout
                    } else {
                        ProviderError::IncompleteResponse
                    }
                })?;
                total = total.saturating_add(chunk.len());
                if total > self.config.policy.max_body_bytes {
                    return Err(ProviderError::TooLarge);
                }
                for byte in chunk {
                    let Some(frame) = decoder.push(byte)? else {
                        continue;
                    };
                    if frame == "[DONE]" {
                        if !stopped {
                            return Err(ProviderError::IncompleteResponse);
                        }
                        validate_answer(&text)?;
                        return Ok(text);
                    }
                    let value: Value = serde_json::from_str(&frame)
                        .map_err(|_| ProviderError::MalformedResponse)?;
                    check_response(&value)?;
                    let response: Completion = serde_json::from_value(value)
                        .map_err(|_| ProviderError::MalformedResponse)?;
                    // OpenAI's optional final usage frame has no choices.
                    if response.choices.is_empty() {
                        continue;
                    }
                    if stopped {
                        return Err(ProviderError::MalformedResponse);
                    }
                    let choice = only_choice(response.choices)?;
                    let delta = choice.delta.ok_or(ProviderError::MalformedResponse)?;
                    delta.validate()?;
                    // Check termination BEFORE publishing the same frame's content.
                    if choice.finish_reason.is_some() {
                        finish(choice.finish_reason.as_deref())?;
                        stopped = true;
                    }
                    let part = delta
                        .content
                        .map(TextContent::into_text)
                        .transpose()?
                        .unwrap_or_default();
                    if !part.is_empty() {
                        if text.len().saturating_add(part.len()) > MAX_ANSWER_BYTES {
                            return Err(ProviderError::TooLarge);
                        }
                        text.push_str(&part);
                        context
                            .emit("delta", json!({"delta":part}))
                            .await
                            .map_err(|error| {
                                if error == crate::protocol::ErrorCode::ResponseTooLarge {
                                    ProviderError::TooLarge
                                } else {
                                    ProviderError::Cancelled
                                }
                            })?;
                    }
                }
            }
            // A closed socket is NOT successful completion, even after content.
            Err(ProviderError::IncompleteResponse)
        })
        .await
    }
}

impl ModelClient for HttpModelClient {
    fn complete(
        &self,
        messages: Vec<Message>,
        cancellation: Cancellation,
    ) -> BackendFuture<String> {
        let client = self.clone();
        Box::pin(async move {
            client
                .complete_response(messages, None, cancellation)
                .await
                .map_err(Into::into)
        })
    }

    fn stream(&self, messages: Vec<Message>, context: RequestContext) -> BackendFuture<String> {
        let client = self.clone();
        Box::pin(async move {
            client
                .stream_final(messages, context)
                .await
                .map_err(Into::into)
        })
    }
}

pub(crate) fn validate_messages(messages: &[Message]) -> ProviderResult<()> {
    if messages.is_empty()
        || messages.iter().any(|m| {
            !matches!(
                m.role.as_str(),
                "system" | "user" | "assistant" | "developer"
            ) || m.content.trim().is_empty()
        })
    {
        return Err(ProviderError::InvalidInput);
    }
    if messages
        .iter()
        .try_fold(0usize, |n, m| n.checked_add(m.content.len()))
        .is_none_or(|n| n > super::config::MAX_BODY_BYTES)
    {
        return Err(ProviderError::TooLarge);
    }
    Ok(())
}

pub(crate) fn validate_answer(text: &str) -> ProviderResult<()> {
    if text.trim().is_empty() {
        return Err(ProviderError::MalformedResponse);
    }
    if text.len() > MAX_ANSWER_BYTES {
        return Err(ProviderError::TooLarge);
    }
    Ok(())
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    index: usize,
    finish_reason: Option<String>,
    message: Option<Content>,
    delta: Option<Content>,
}

#[derive(Deserialize)]
struct Content {
    content: Option<TextContent>,
    refusal: Option<String>,
    tool_calls: Option<Value>,
    function_call: Option<Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TextContent {
    Text(String),
    Parts(Vec<TextPart>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TextPart {
    Text(String),
    Block {
        #[serde(rename = "type")]
        kind: String,
        text: Option<String>,
    },
}

impl TextContent {
    fn into_text(self) -> ProviderResult<String> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Parts(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        TextPart::Text(value) => text.push_str(&value),
                        TextPart::Block { kind, text: value }
                            if matches!(kind.as_str(), "text" | "output_text") =>
                        {
                            text.push_str(&value.ok_or(ProviderError::MalformedResponse)?);
                        }
                        // Reasoning/image/audio parts are never public answer deltas.
                        TextPart::Block { .. } => {}
                    }
                }
                Ok(text)
            }
        }
    }
}

impl Content {
    fn validate(&self) -> ProviderResult<()> {
        if self.refusal.as_ref().is_some_and(|s| !s.is_empty()) {
            return Err(ProviderError::Refused);
        }
        if self.tool_calls.is_some() || self.function_call.is_some() {
            return Err(ProviderError::MalformedResponse);
        }
        Ok(())
    }
}

fn only_choice(mut choices: Vec<Choice>) -> ProviderResult<Choice> {
    if choices.len() != 1 || choices[0].index != 0 {
        return Err(ProviderError::MalformedResponse);
    }
    Ok(choices.remove(0))
}

fn finish(reason: Option<&str>) -> ProviderResult<()> {
    match reason {
        Some("stop") => Ok(()),
        Some("content_filter") => Err(ProviderError::Refused),
        _ => Err(ProviderError::IncompleteResponse),
    }
}

fn check_response(value: &Value) -> ProviderResult<()> {
    if value.get("error").is_some_and(|v| !v.is_null()) {
        return Err(ProviderError::Http);
    }
    if output_limited(value, false) {
        return Err(ProviderError::IncompleteResponse);
    }
    Ok(())
}

// Preserve legacy nested provider termination metadata checks without exposing it.
fn output_limited(value: &Value, incomplete: bool) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            let key = key.trim().to_ascii_lowercase().replace('-', "_");
            let nested = incomplete || matches!(key.as_str(), "incomplete" | "incomplete_details");
            let reason = matches!(
                key.as_str(),
                "finish_reason" | "native_finish_reason" | "stop_reason" | "termination_reason"
            ) || (nested && key == "reason");
            (reason
                && value.as_str().is_some_and(|s| {
                    matches!(
                        s.trim()
                            .to_ascii_lowercase()
                            .replace(['-', ' '], "_")
                            .as_str(),
                        "length"
                            | "max_completion_tokens"
                            | "max_output_tokens"
                            | "max_tokens"
                            | "output_limit"
                            | "token_limit"
                    )
                }))
                || output_limited(value, nested)
        }),
        Value::Array(values) => values.iter().any(|v| output_limited(v, incomplete)),
        _ => false,
    }
}
