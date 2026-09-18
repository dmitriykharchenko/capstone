mod provider_support;

use std::time::Duration;

use provider_support::*;
use research_backend::{providers::*, runtime::Cancellation};
use serde::Deserialize;
use serde_json::json;

fn model_config(url: &str) -> ModelConfig {
    ModelConfig::new(
        "override/model".into(),
        format!("{url}/"),
        "offline-test".into(),
    )
}

fn embedding_config(url: &str) -> EmbeddingConfig {
    EmbeddingConfig::new(
        "override/embedding".into(),
        url.into(),
        "offline-test".into(),
    )
}

#[tokio::test]
async fn chat_request_preserves_endpoint_model_options_without_storage_fields() {
    let mut fake = FakeHttp::new(vec![
        Plan::json(completion("answer")),
        Plan::json(completion("second")),
    ])
    .await;
    let client = HttpModelClient::new(model_config(&fake.url)).unwrap();
    assert_eq!(
        client
            .complete(messages("question"), Cancellation::default())
            .await
            .unwrap(),
        "answer"
    );
    let request = fake.request().await;
    assert_eq!(request.path, "/compatible/v1/chat/completions");
    assert!(request.has_authorization);
    assert_eq!(
        request.body,
        json!({"model":"override/model","temperature":0.2,"stream":false,
        "messages":[{"role":"user","content":"question"}]})
    );
    let mut config = model_config(&fake.url);
    config.max_tokens = Some(120);
    config.temperature = 0.7;
    HttpModelClient::new(config)
        .unwrap()
        .complete(messages("next"), Cancellation::default())
        .await
        .unwrap();
    let request = fake.request().await;
    assert_eq!(request.body["max_tokens"], 120);
    assert_eq!(request.body["temperature"], 0.7);
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct PlanDto {
    subqueries: Vec<String>,
}

#[tokio::test]
async fn structured_calls_validate_typed_json_and_request_backend_schema() {
    let mut fake = FakeHttp::new(vec![
        Plan::json(completion(r#"{"subqueries":["one","two"]}"#)),
        Plan::json(completion(r#"{"subqueries":42}"#)),
        Plan::json(completion("```json\n{}\n```")),
    ])
    .await;
    let client = HttpModelClient::new(model_config(&fake.url)).unwrap();
    let format = ResponseFormat::json_schema("plan".into(), json!({"type":"object","properties":{"subqueries":{"type":"array","items":{"type":"string"}}},"required":["subqueries"],"additionalProperties":false})).unwrap();
    let plan: PlanDto = client
        .complete_json(messages("JSON plan"), format, Cancellation::default())
        .await
        .unwrap();
    assert_eq!(plan.subqueries, ["one", "two"]);
    let request = fake.request().await;
    assert_eq!(
        request.body["response_format"]["json_schema"]["strict"],
        true
    );
    assert_eq!(request.body["response_format"]["type"], "json_schema");
    for _ in 0..2 {
        assert_eq!(
            client
                .complete_json::<PlanDto>(
                    messages("JSON plan"),
                    ResponseFormat::JsonObject,
                    Cancellation::default()
                )
                .await,
            Err(ProviderError::MalformedResponse)
        );
        assert_eq!(
            fake.request().await.body["response_format"],
            json!({"type":"json_object"})
        );
    }
}

#[tokio::test]
async fn chat_rejects_malformed_refused_and_nested_truncated_responses() {
    let mut nested = completion("partial");
    nested["provider_metadata"] = json!({"incomplete_details":{"reason":"MAX OUTPUT TOKENS"}});
    let mut filtered = completion("unsafe");
    filtered["choices"][0]["finish_reason"] = json!("content_filter");
    let mut refusal = completion("no");
    refusal["choices"][0]["message"]["refusal"] = json!("private refusal");
    let mut length = completion("partial");
    length["choices"][0]["finish_reason"] = json!("length");
    for (value, error) in [
        (json!({"choices":[]}), ProviderError::MalformedResponse),
        (
            json!({"choices":[{"index":1,"message":{"content":"wrong choice"},"finish_reason":"stop"}]}),
            ProviderError::MalformedResponse,
        ),
        (
            json!({"choices":[{"index":0,"message":{"content":"first"},"finish_reason":"stop"},{"index":1,"message":{"content":"second"},"finish_reason":"stop"}]}),
            ProviderError::MalformedResponse,
        ),
        (
            json!({"choices":[{"index":0,"message":{"content":7},"finish_reason":"stop"}]}),
            ProviderError::MalformedResponse,
        ),
        (completion("  "), ProviderError::MalformedResponse),
        (
            json!({"error":{"message":"private diagnostic"}}),
            ProviderError::Http,
        ),
        (nested, ProviderError::IncompleteResponse),
        (length, ProviderError::IncompleteResponse),
        (filtered, ProviderError::Refused),
        (refusal, ProviderError::Refused),
    ] {
        let fake = FakeHttp::new(vec![Plan::json(value)]).await;
        assert_eq!(
            HttpModelClient::new(model_config(&fake.url))
                .unwrap()
                .complete_response(messages("q"), None, Cancellation::default())
                .await,
            Err(error)
        );
    }
}

#[tokio::test]
async fn embedding_batch_preserves_order_and_numeric_vectors() {
    let mut fake = FakeHttp::new(vec![Plan::json(json!({"data":[
        {"index":1,"embedding":[3,4.5]}, {"index":0,"embedding":[1.0,-2]}]}))])
    .await;
    let mut config = embedding_config(&fake.url);
    config.expected_dimensions = Some(2);
    let client = HttpEmbeddingClient::new(config).unwrap();
    assert_eq!(
        client
            .embed(
                vec!["first".into(), "second".into()],
                Cancellation::default()
            )
            .await
            .unwrap(),
        vec![vec![1.0, -2.0], vec![3.0, 4.5]]
    );
    let request = fake.request().await;
    assert_eq!(request.path, "/compatible/v1/embeddings");
    assert_eq!(
        request.body,
        json!({"model":"override/embedding","input":["first","second"],"encoding_format":"float"})
    );
}

#[tokio::test]
async fn text_content_parts_preserve_text_but_not_reasoning() {
    let mut response = completion("placeholder");
    response["choices"][0]["message"]["content"] = json!([
        "one", {"type":"text","text":" two"}, {"type":"reasoning","text":"PRIVATE"}
    ]);
    let fake = FakeHttp::new(vec![Plan::json(response)]).await;
    assert_eq!(
        HttpModelClient::new(model_config(&fake.url))
            .unwrap()
            .complete_response(messages("q"), None, Cancellation::default())
            .await
            .unwrap(),
        "one two"
    );
}

#[tokio::test]
async fn all_legacy_nested_output_limit_reasons_are_rejected() {
    for reason in [
        "length",
        "max_completion_tokens",
        "max_output_tokens",
        "max_tokens",
        "output_limit",
        "token_limit",
    ] {
        let mut response = completion("partial");
        response["metadata"] = json!([{"nested":{"termination_reason":reason}}]);
        let fake = FakeHttp::new(vec![Plan::json(response)]).await;
        assert_eq!(
            HttpModelClient::new(model_config(&fake.url))
                .unwrap()
                .complete_response(messages("q"), None, Cancellation::default())
                .await,
            Err(ProviderError::IncompleteResponse)
        );
    }
}

#[tokio::test]
async fn embedding_rejects_bad_counts_indices_dimensions_and_numbers() {
    for data in [
        json!([]),
        json!([{"index":0,"embedding":[1]}]),
        json!([{"index":0,"embedding":[1]},{"index":0,"embedding":[2]}]),
        json!([{"index":0,"embedding":[1]},{"index":2,"embedding":[2]}]),
        json!([{"index":0,"embedding":[1]},{"index":1,"embedding":[1,2]}]),
        json!([{"index":0,"embedding":[]},{"index":1,"embedding":[]}]),
        json!([{"index":0,"embedding":[true]},{"index":1,"embedding":[2]}]),
        json!([{"index":0,"embedding":["1"]},{"index":1,"embedding":[2]}]),
        json!([{"index":0,"embedding":[null]},{"index":1,"embedding":[2]}]),
        json!([{"index":-1,"embedding":[1]},{"index":1,"embedding":[2]}]),
        json!([{"embedding":[1]},{"index":1,"embedding":[2]}]),
    ] {
        let fake = FakeHttp::new(vec![Plan::json(json!({"data":data}))]).await;
        assert_eq!(
            HttpEmbeddingClient::new(embedding_config(&fake.url))
                .unwrap()
                .embed_batch(vec!["a".into(), "b".into()], Cancellation::default())
                .await,
            Err(ProviderError::MalformedResponse)
        );
    }
    for body in [
        r#"{"data":[{"index":0,"embedding":[1e999]}]}"#,
        r#"{"data":[{"index":0,"embedding":[NaN]}]}"#,
        "not json",
    ] {
        let fake = FakeHttp::new(vec![Plan::raw(
            body.as_bytes().to_vec(),
            "application/json",
        )])
        .await;
        assert_eq!(
            HttpEmbeddingClient::new(embedding_config(&fake.url))
                .unwrap()
                .embed_batch(vec!["a".into()], Cancellation::default())
                .await,
            Err(ProviderError::MalformedResponse)
        );
    }
    let fake = FakeHttp::new(vec![Plan::json(
        json!({"data":[{"index":0,"embedding":[1,2]}]}),
    )])
    .await;
    let mut config = embedding_config(&fake.url);
    config.expected_dimensions = Some(3);
    assert_eq!(
        HttpEmbeddingClient::new(config)
            .unwrap()
            .embed_batch(vec!["a".into()], Cancellation::default())
            .await,
        Err(ProviderError::MalformedResponse)
    );
}

#[tokio::test]
async fn empty_and_invalid_inputs_do_not_contact_provider() {
    let mut fake = FakeHttp::new(vec![Plan::json(completion("unused"))]).await;
    let client = HttpEmbeddingClient::new(embedding_config(&fake.url)).unwrap();
    assert_eq!(
        client
            .embed_batch(vec![], Cancellation::default())
            .await
            .unwrap(),
        Vec::<Vec<f64>>::new()
    );
    assert_eq!(
        client
            .embed_batch(vec![" \n".into()], Cancellation::default())
            .await,
        Err(ProviderError::InvalidInput)
    );
    let client = HttpModelClient::new(model_config(&fake.url)).unwrap();
    assert_eq!(
        client
            .complete_response(vec![], None, Cancellation::default())
            .await,
        Err(ProviderError::InvalidInput)
    );
    let mut invalid = messages("q");
    invalid[0].role = "tool".into();
    assert_eq!(
        client
            .complete_response(invalid, None, Cancellation::default())
            .await,
        Err(ProviderError::InvalidInput)
    );
    assert_eq!(fake.remaining_requests(), 0);
}

#[tokio::test]
async fn retries_are_bounded_and_errors_are_sanitized() {
    let error_plan = |status| {
        let mut p = Plan::json(json!({"error":{"message":"PRIVATE_PROVIDER_DETAIL"}}));
        p.status = status;
        p
    };
    let mut fake = FakeHttp::new(vec![
        error_plan(429),
        error_plan(503),
        Plan::json(completion("recovered")),
    ])
    .await;
    let client = HttpModelClient::new(model_config(&fake.url)).unwrap();
    assert_eq!(
        client
            .complete_response(messages("q"), None, Cancellation::default())
            .await
            .unwrap(),
        "recovered"
    );
    for _ in 0..3 {
        fake.request().await;
    }
    for (status, expected, count) in [
        (429, ProviderError::RateLimited, 3),
        (500, ProviderError::Http, 3),
        (401, ProviderError::Authentication, 1),
        (400, ProviderError::Http, 1),
        (302, ProviderError::Http, 1),
    ] {
        let mut fake = FakeHttp::new((0..count).map(|_| error_plan(status)).collect()).await;
        let error = HttpModelClient::new(model_config(&fake.url))
            .unwrap()
            .complete_response(messages("q"), None, Cancellation::default())
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("PRIVATE_PROVIDER_DETAIL"));
        for _ in 0..count {
            fake.request().await;
        }
        assert_eq!(fake.remaining_requests(), 0);
    }
}

#[tokio::test]
async fn body_limits_apply_to_chunked_and_advertised_responses() {
    for advertised in [None, Some(10_000)] {
        let mut plan = Plan::raw(vec![b'x'; 128], "application/json");
        plan.advertised_length = advertised;
        let fake = FakeHttp::new(vec![plan]).await;
        let mut config = model_config(&fake.url);
        config.policy.max_body_bytes = 64;
        assert_eq!(
            HttpModelClient::new(config)
                .unwrap()
                .complete_response(messages("q"), None, Cancellation::default())
                .await,
            Err(ProviderError::TooLarge)
        );
    }
}

#[tokio::test]
async fn request_and_nonstream_answer_limits_are_checked() {
    let mut fake = FakeHttp::new(vec![Plan::json(completion(&"x".repeat(512 * 1024 + 1)))]).await;
    let client = HttpModelClient::new(model_config(&fake.url)).unwrap();
    assert_eq!(
        client
            .complete_response(
                messages(&"x".repeat(8 * 1024 * 1024 + 1)),
                None,
                Cancellation::default()
            )
            .await,
        Err(ProviderError::TooLarge)
    );
    assert_eq!(fake.remaining_requests(), 0);
    assert_eq!(
        client
            .complete_response(messages("q"), None, Cancellation::default())
            .await,
        Err(ProviderError::TooLarge)
    );
}

#[tokio::test]
async fn whole_operation_timeout_bounds_headers_body_and_retries() {
    let mut slow = Plan::json(completion("too late"));
    slow.delay = Duration::from_millis(200);
    let fake = FakeHttp::new(vec![slow]).await;
    let mut config = model_config(&fake.url);
    config.policy.timeout = Duration::from_millis(30);
    assert_eq!(
        HttpModelClient::new(config)
            .unwrap()
            .complete_response(messages("q"), None, Cancellation::default())
            .await,
        Err(ProviderError::Timeout)
    );
    let mut hang = Plan::raw(vec![b'{'], "application/json");
    hang.hang = true;
    let fake = FakeHttp::new(vec![hang]).await;
    let mut config = embedding_config(&fake.url);
    config.policy.timeout = Duration::from_millis(30);
    assert_eq!(
        HttpEmbeddingClient::new(config)
            .unwrap()
            .embed_batch(vec!["q".into()], Cancellation::default())
            .await,
        Err(ProviderError::Timeout)
    );
    let mut busy = Plan::json(json!({}));
    busy.status = 429;
    let mut fake = FakeHttp::new(vec![busy]).await;
    let mut config = model_config(&fake.url);
    config.policy.timeout = Duration::from_millis(30);
    assert_eq!(
        HttpModelClient::new(config)
            .unwrap()
            .complete_response(messages("q"), None, Cancellation::default())
            .await,
        Err(ProviderError::Timeout)
    );
    assert_eq!(fake.remaining_requests(), 1);
}

#[test]
fn invalid_configuration_is_rejected_before_network() {
    for url in [
        "not a url",
        "ftp://localhost/v1",
        "http://example.com/v1",
        "https://example.com/v1?key=x",
        "https://example.com/v1#fragment",
        "https://user@example.com/v1",
    ] {
        assert!(matches!(
            HttpModelClient::new(model_config(url)),
            Err(ProviderError::Configuration)
        ));
    }
    let mut config = model_config("https://api.openai.com/v1");
    config.max_tokens = Some(0);
    assert!(HttpModelClient::new(config).is_err());
    let mut config = model_config("https://api.openai.com/v1");
    config.temperature = f64::NAN;
    assert!(HttpModelClient::new(config).is_err());
    let mut config = model_config("https://api.openai.com/v1");
    config.policy.max_retries = 3;
    assert!(HttpModelClient::new(config).is_err());
    let mut config = model_config("https://api.openai.com/v1");
    config.policy.timeout = Duration::ZERO;
    assert!(HttpModelClient::new(config).is_err());
    let mut config = embedding_config("https://api.openai.com/v1");
    config.expected_dimensions = Some(0);
    assert!(HttpEmbeddingClient::new(config).is_err());
    assert!(ResponseFormat::json_schema("invalid name".into(), json!({})).is_err());
}
