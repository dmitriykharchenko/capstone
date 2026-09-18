mod provider_support;

use provider_support::*;
use research_backend::{
    providers::*,
    registry::{BackendFuture, CommandHandler},
    runtime::{Cancellation, RequestContext},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

struct HttpHandler {
    model: HttpModelClient,
    outcomes: mpsc::UnboundedSender<ProviderResult<String>>,
}

impl CommandHandler for HttpHandler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let model = self.model.clone();
        let outcomes = self.outcomes.clone();
        Box::pin(async move {
            let result = if params["private"] == true {
                model
                    .complete_response(messages("private planner"), None, context.cancellation())
                    .await
            } else {
                model
                    .stream_final(messages("final composition"), context)
                    .await
            };
            outcomes.send(result.clone()).unwrap();
            result
                .map(|reply| json!({"reply":reply}))
                .map_err(Into::into)
        })
    }
}

fn handler(
    url: &str,
) -> (
    Arc<HttpHandler>,
    mpsc::UnboundedReceiver<ProviderResult<String>>,
) {
    let (outcomes, receiver) = mpsc::unbounded_channel();
    let config = ModelConfig::new("model".into(), url.into(), "offline-test".into());
    (
        Arc::new(HttpHandler {
            model: HttpModelClient::new(config).unwrap(),
            outcomes,
        }),
        receiver,
    )
}

async fn stream(plan: Plan) -> (Vec<Value>, ProviderResult<String>) {
    let mut fake = FakeHttp::new(vec![plan]).await;
    let (handler, mut outcomes) = handler(&fake.url);
    let mut runtime = RuntimeClient::new(handler);
    runtime.request("chat_stream", json!({})).await;
    let mut output = vec![];
    loop {
        let item = runtime.next().await;
        let terminal = item["type"] != "event";
        output.push(item);
        if terminal {
            break;
        }
    }
    assert_eq!(fake.request().await.body["stream"], true);
    assert!(runtime.finish().await.is_empty());
    (output, outcomes.recv().await.unwrap())
}

#[tokio::test]
async fn split_sse_utf8_crlf_multiline_comments_and_usage_emit_only_final_content() {
    let body = format!(
        "\u{feff}: keepalive\r\n\r\n{}data: {{\r\ndata: \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hé🙂\"}},\"finish_reason\":null}}]}}\r\n\r\n{}{}data: {{\"choices\":[],\"usage\":{{\"total_tokens\":12}}}}\r\n\r\ndata: [DONE]\r\n\r\n",
        delta(
            json!({"role":"assistant","reasoning":"PRIVATE_REASONING","reasoning_details":[{"text":"PRIVATE_DETAIL"}]}),
            Value::Null
        ),
        delta(json!({"content":" world"}), Value::Null),
        delta(json!({}), json!("stop"))
    );
    for chunk_size in [1, 2, 7, 1024] {
        let (output, result) = stream(Plan::sse(body.clone(), chunk_size)).await;
        assert_eq!(result.unwrap(), "hé🙂 world");
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["event"], "delta");
        assert_eq!(output[0]["data"], json!({"delta":"hé🙂"}));
        assert_eq!(output[1]["data"], json!({"delta":" world"}));
        assert_eq!(output[2]["data"]["reply"], "hé🙂 world");
        for (sequence, item) in output.iter().enumerate() {
            assert_eq!(item["seq"], sequence);
            assert!(!item.to_string().contains("PRIVATE"));
        }
    }
}

#[tokio::test]
async fn bare_cr_sse_is_supported() {
    let body = format!(
        "{}{}data: [DONE]\n\n",
        delta(json!({"content":"ok"}), Value::Null),
        delta(json!({}), json!("stop"))
    )
    .replace('\n', "\r");
    assert_eq!(stream(Plan::sse(body, 1)).await.1.unwrap(), "ok");
}

#[tokio::test]
async fn stream_requires_stop_and_done_and_never_retries_after_partial_output() {
    let partial = delta(json!({"content":"partial"}), Value::Null);
    for body in [
        partial.clone(),
        format!("{partial}data: [DONE]\n\n"),
        format!("{partial}{}", delta(json!({}), json!("stop"))),
        format!(
            "{partial}{}data: [DONE]\n\n",
            delta(json!({"content":"discard"}), json!("length"))
        ),
        format!(
            "{partial}data: {{\"choices\":[],\"metadata\":{{\"incomplete_details\":{{\"reason\":\"max_tokens\"}}}}}}\n\n"
        ),
    ] {
        let (output, result) = stream(Plan::sse(body, 3)).await;
        assert_eq!(result, Err(ProviderError::IncompleteResponse));
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["data"], json!({"delta":"partial"}));
        assert_eq!(output[1]["type"], "error");
        assert_eq!(output[1]["error"]["message"], "Backend operation failed.");
    }
}

#[tokio::test]
async fn malformed_streams_refusals_and_embedded_errors_fail_safely() {
    for (body, error) in [
        (
            "data: not-json\n\n".into(),
            ProviderError::MalformedResponse,
        ),
        (
            "data: {\"choices\":{}}\n\n".into(),
            ProviderError::MalformedResponse,
        ),
        (
            "data: {\"error\":{\"message\":\"PRIVATE_PROVIDER_DETAIL\"}}\n\n".into(),
            ProviderError::Http,
        ),
        (
            delta(json!({"content":42}), Value::Null),
            ProviderError::MalformedResponse,
        ),
        (
            delta(json!({"refusal":"PRIVATE_REFUSAL"}), Value::Null),
            ProviderError::Refused,
        ),
        (
            delta(json!({"tool_calls":[]}), Value::Null),
            ProviderError::MalformedResponse,
        ),
        (
            format!("{}data: [DONE]\n\n", delta(json!({}), json!("stop"))),
            ProviderError::MalformedResponse,
        ),
        (
            format!(
                "{}{}",
                delta(json!({"content":"done"}), json!("stop")),
                delta(json!({"content":"late"}), Value::Null)
            ),
            ProviderError::MalformedResponse,
        ),
    ] {
        let (output, result) = stream(Plan::sse(body, 5)).await;
        assert_eq!(result, Err(error));
        assert_eq!(output.last().unwrap()["type"], "error");
        assert!(!json!(output).to_string().contains("PRIVATE"));
    }
    let (_, result) = stream(Plan::raw(
        vec![b'd', b'a', b't', b'a', b':', b' ', 0xff, b'\n', b'\n'],
        "text/event-stream",
    ))
    .await;
    assert_eq!(result, Err(ProviderError::MalformedResponse));
    assert_eq!(
        stream(Plan::json(completion("not SSE"))).await.1,
        Err(ProviderError::MalformedResponse)
    );
}

#[tokio::test]
async fn oversized_sse_frames_and_accumulated_answer_are_bounded() {
    let (output, result) =
        stream(Plan::sse(format!("data: {}", "x".repeat(256 * 1024)), 4096)).await;
    assert_eq!(result, Err(ProviderError::TooLarge));
    assert_eq!(output.len(), 1);
    let body = delta(json!({"content":"x".repeat(128*1024)}), Value::Null).repeat(5);
    let (output, result) = stream(Plan::sse(body, 4096)).await;
    assert_eq!(result, Err(ProviderError::TooLarge));
    assert_eq!(output.len(), 5);
}

#[tokio::test]
async fn nonstream_intermediate_call_never_emits_provider_reasoning_or_deltas() {
    let mut response = completion(r#"{"subqueries":["one","two"]}"#);
    response["choices"][0]["message"]["reasoning"] = json!("PRIVATE_REASONING");
    let fake = FakeHttp::new(vec![Plan::json(response)]).await;
    let (handler, mut outcomes) = handler(&fake.url);
    let mut runtime = RuntimeClient::new(handler);
    runtime
        .request("chat_stream", json!({"private":true}))
        .await;
    let item = runtime.next().await;
    assert_eq!(item["type"], "result");
    assert_eq!(item["seq"], 0);
    assert!(!item.to_string().contains("PRIVATE"));
    assert!(outcomes.recv().await.unwrap().is_ok());
    assert!(runtime.finish().await.is_empty());
}

#[tokio::test]
async fn abrupt_http_body_truncation_is_not_success() {
    let mut plan = Plan::sse(delta(json!({"content":"partial"}), Value::Null), 1024);
    plan.advertised_length = Some(10000);
    let (output, result) = stream(plan).await;
    assert_eq!(result, Err(ProviderError::IncompleteResponse));
    assert_eq!(output.last().unwrap()["type"], "error");
}

#[tokio::test]
async fn stalled_stream_and_total_stream_body_have_bounds() {
    for timeout in [false, true] {
        let mut plan = Plan::sse(delta(json!({"content":"provisional"}), Value::Null), 4);
        plan.hang = timeout;
        let fake = FakeHttp::new(vec![plan]).await;
        let mut config = ModelConfig::new("model".into(), fake.url.clone(), "offline-test".into());
        if timeout {
            config.policy.timeout = Duration::from_millis(100);
        } else {
            config.policy.max_body_bytes = 16;
        }
        let (tx, mut outcomes) = mpsc::unbounded_channel();
        let mut runtime = RuntimeClient::new(Arc::new(HttpHandler {
            model: HttpModelClient::new(config).unwrap(),
            outcomes: tx,
        }));
        runtime.request("chat_stream", json!({})).await;
        loop {
            if runtime.next().await["type"] == "error" {
                break;
            }
        }
        assert_eq!(
            outcomes.recv().await.unwrap(),
            Err(if timeout {
                ProviderError::Timeout
            } else {
                ProviderError::TooLarge
            })
        );
        assert!(runtime.finish().await.is_empty());
    }
}

#[tokio::test]
async fn cancellation_closes_stream_and_has_exactly_one_terminal_no_late_events() {
    let mut plan = Plan::sse(delta(json!({"content":"provisional"}), Value::Null), 8);
    plan.hang = true;
    let mut fake = FakeHttp::new(vec![plan]).await;
    let (handler, _) = handler(&fake.url);
    let mut runtime = RuntimeClient::new(handler);
    runtime.request("chat_stream", json!({})).await;
    assert_eq!(runtime.next().await["data"], json!({"delta":"provisional"}));
    runtime
        .send(json!({"v":1,"type":"request","id":"health","command":"health","params":{}}))
        .await;
    assert_eq!(runtime.next().await["id"], "health");
    runtime.send(json!({"v":1,"type":"cancel","id":"p"})).await;
    assert_eq!(runtime.next().await["type"], "cancelled");
    assert_eq!(runtime.next().await["accepted"], true);
    fake.wait_closed().await;
    assert!(runtime.finish().await.is_empty());
}

struct DetachedHandler {
    model: HttpModelClient,
    embedding: HttpEmbeddingClient,
    outcomes: mpsc::UnboundedSender<ProviderError>,
    tokens: mpsc::UnboundedSender<Cancellation>,
}

impl CommandHandler for DetachedHandler {
    fn call(&self, params: Value, context: RequestContext) -> BackendFuture<Value> {
        let model = self.model.clone();
        let embedding = self.embedding.clone();
        let outcomes = self.outcomes.clone();
        let tokens = self.tokens.clone();
        Box::pin(async move {
            let cancellation = context.cancellation();
            tokens.send(cancellation.clone()).unwrap();
            // Independent worker is not aborted by runtime: verifies token polling.
            tokio::spawn(async move {
                let error = if params["embedding"] == true {
                    embedding
                        .embed_batch(vec!["q".into()], cancellation)
                        .await
                        .unwrap_err()
                } else {
                    model
                        .complete_response(messages("q"), None, cancellation)
                        .await
                        .unwrap_err()
                };
                outcomes.send(error).unwrap();
            });
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn cloned_cancellation_interrupts_model_and_embedding_reads_and_prevents_new_calls() {
    for is_embedding in [false, true] {
        let mut plan = Plan::raw(vec![b'{'], "application/json");
        plan.hang = true;
        let mut fake = FakeHttp::new(vec![plan]).await;
        let model = HttpModelClient::new(ModelConfig::new(
            "model".into(),
            fake.url.clone(),
            "offline-test".into(),
        ))
        .unwrap();
        let embedding = HttpEmbeddingClient::new(EmbeddingConfig::new(
            "model".into(),
            fake.url.clone(),
            "offline-test".into(),
        ))
        .unwrap();
        let (tx, mut outcomes) = mpsc::unbounded_channel();
        let (tokens, mut rx) = mpsc::unbounded_channel();
        let mut runtime = RuntimeClient::new(Arc::new(DetachedHandler {
            model: model.clone(),
            embedding: embedding.clone(),
            outcomes: tx,
            tokens,
        }));
        runtime
            .request("chat", json!({"embedding":is_embedding}))
            .await;
        fake.request().await;
        let token = rx.recv().await.unwrap();
        runtime.send(json!({"v":1,"type":"cancel","id":"p"})).await;
        assert_eq!(runtime.next().await["type"], "cancelled");
        assert_eq!(runtime.next().await["accepted"], true);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), outcomes.recv())
                .await
                .unwrap(),
            Some(ProviderError::Cancelled)
        );
        fake.wait_closed().await;
        assert_eq!(
            model
                .complete_response(messages("q"), None, token.clone())
                .await,
            Err(ProviderError::Cancelled)
        );
        assert_eq!(
            embedding.embed_batch(vec!["q".into()], token).await,
            Err(ProviderError::Cancelled)
        );
        assert_eq!(fake.remaining_requests(), 0);
        assert!(runtime.finish().await.is_empty());
    }
}
