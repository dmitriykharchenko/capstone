use std::{future::Future, time::Duration};

use futures_util::StreamExt;
use reqwest::{Client, Response, Url, header};
use serde_json::Value;

use crate::runtime::Cancellation;

use super::{ProviderError, ProviderResult, RequestPolicy, config::MAX_BODY_BYTES};

#[derive(Clone)]
pub(crate) struct Transport {
    client: Client,
    endpoint: Url,
    authorization: header::HeaderValue,
    pub policy: RequestPolicy,
}

impl Transport {
    pub fn new(base: &str, route: &str, key: &str, policy: RequestPolicy) -> ProviderResult<Self> {
        policy.validate()?;
        let mut url = Url::parse(base).map_err(|_| ProviderError::Configuration)?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if !(url.scheme() == "https" || (url.scheme() == "http" && local))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || key.trim().is_empty()
        {
            return Err(ProviderError::Configuration);
        }
        let path = format!("{}/{}", url.path().trim_end_matches('/'), route);
        url.set_path(&path);
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| ProviderError::Configuration)?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(policy.timeout.min(Duration::from_secs(10)))
            .timeout(policy.timeout)
            .build()
            .map_err(|_| ProviderError::Configuration)?;
        Ok(Self {
            client,
            endpoint: url,
            authorization,
            policy,
        })
    }

    pub async fn post(&self, body: &Value, stream: bool) -> ProviderResult<Response> {
        let bytes = serde_json::to_vec(body).map_err(|_| ProviderError::InvalidInput)?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(ProviderError::TooLarge);
        }
        for attempt in 0..=self.policy.max_retries {
            let response = self
                .client
                .post(self.endpoint.clone())
                .header(header::AUTHORIZATION, self.authorization.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::ACCEPT,
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(bytes.clone())
                .send()
                .await;
            match response {
                Ok(response) => {
                    let code = response.status().as_u16();
                    if response.status().is_success() {
                        return Ok(response);
                    }
                    // Never read or echo error bodies; unbounded/secret-bearing errors are dropped.
                    if attempt == self.policy.max_retries
                        || !(matches!(code, 408 | 409 | 429) || code >= 500)
                    {
                        return Err(match code {
                            401 | 403 => ProviderError::Authentication,
                            429 => ProviderError::RateLimited,
                            _ => ProviderError::Http,
                        });
                    }
                }
                Err(error) => {
                    if error.is_timeout() {
                        return Err(ProviderError::Timeout);
                    }
                    // Do not replay requests that may already have reached the provider.
                    if !error.is_connect() || attempt == self.policy.max_retries {
                        return Err(ProviderError::Transport);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
        }
        unreachable!("bounded retry loop always returns")
    }

    pub async fn json(&self, body: &Value) -> ProviderResult<Value> {
        let response = self.post(body, false).await?;
        if response
            .content_length()
            .is_some_and(|n| n > self.policy.max_body_bytes as u64)
        {
            return Err(ProviderError::TooLarge);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(network_error)?;
            if bytes.len().saturating_add(chunk.len()) > self.policy.max_body_bytes {
                return Err(ProviderError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ProviderError::MalformedResponse)
    }
}

pub(crate) fn network_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Transport
    }
}

pub(crate) fn check(cancellation: &Cancellation) -> ProviderResult<()> {
    cancellation.check().map_err(|_| ProviderError::Cancelled)
}

/// Cancellation is cooperative for cloned tokens too, not just dispatcher aborts.
pub(crate) async fn guarded<T>(
    cancellation: &Cancellation,
    timeout: Duration,
    operation: impl Future<Output = ProviderResult<T>>,
) -> ProviderResult<T> {
    check(cancellation)?;
    let mut ticks = tokio::time::interval(Duration::from_millis(10));
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(operation, deadline);
    loop {
        tokio::select! {
            biased;
            _ = ticks.tick() => check(cancellation)?,
            _ = &mut deadline => return Err(ProviderError::Timeout),
            result = &mut operation => { check(cancellation)?; return result; }
        }
    }
}
