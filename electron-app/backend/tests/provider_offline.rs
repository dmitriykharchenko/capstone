mod provider_support;

use provider_support::*;
use research_backend::{
    providers::*,
    registry::{BackendFuture, CommandHandler},
    runtime::{Cancellation, RequestContext},
};
use serde_json::{Value, json};
use std::sync::Arc;

struct MockHandler;
impl CommandHandler for MockHandler {
    fn call(&self, _: Value, context: RequestContext) -> BackendFuture<Value> {
        Box::pin(async move {
            MockModelClient
                .stream(messages("hello world"), context)
                .await
                .map(|reply| json!({"reply":reply}))
        })
    }
}

#[tokio::test]
async fn explicit_offline_mock_preserves_reply_and_stream_parity() {
    let reply = MockModelClient
        .complete(messages("hello world"), Cancellation::default())
        .await
        .unwrap();
    assert_eq!(
        reply,
        "[offline mock reply] No API key is configured, so this is a deterministic response. You said: hello world"
    );
    let mut runtime = RuntimeClient::new(Arc::new(MockHandler));
    runtime.request("chat_stream", json!({})).await;
    let mut streamed = String::new();
    loop {
        let event = runtime.next().await;
        if event["type"] == "result" {
            assert_eq!(event["data"]["reply"], reply);
            break;
        }
        streamed.push_str(event["data"]["delta"].as_str().unwrap());
    }
    assert_eq!(streamed, reply);
    assert!(runtime.finish().await.is_empty());
}

#[tokio::test]
async fn offline_stage_responses_match_legacy_planner_coverage_composer_metadata() {
    for (stage, input, expected) in [
        (
            "MULTIHOP_PLANNER",
            "Question",
            json!({"subqueries":["Evidence supporting Question","Evidence limiting or contrasting Question"]}),
        ),
        (
            "MULTIHOP_COVERAGE",
            "review",
            json!({"complete":true,"follow_up_subqueries":[]}),
        ),
        (
            "metadata",
            "METADATA_EXTRACTION_INPUT\n<paper_text>\nA paper\nbody\n</paper_text>",
            json!({"title":"A paper","authors":["Unknown author"],"topics":["research","paper","local corpus"]}),
        ),
    ] {
        let mut input_messages = messages(stage);
        input_messages.extend(messages(input));
        let reply = MockModelClient
            .complete(input_messages, Cancellation::default())
            .await
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&reply).unwrap(), expected);
    }
    let mut composer = messages("MULTIHOP_COMPOSER");
    composer.extend(messages(r#"{"original_question":" Question "}"#));
    assert_eq!(
        MockModelClient
            .complete(composer, Cancellation::default())
            .await
            .unwrap(),
        "[offline mock reply] Question"
    );
    let mut subanswer = messages("MULTIHOP_SUBANSWER");
    subanswer.extend(messages(" evidence "));
    assert_eq!(
        MockModelClient
            .complete(subanswer, Cancellation::default())
            .await
            .unwrap(),
        "[offline mock evidence summary] evidence"
    );
}

#[tokio::test]
async fn local_embeddings_match_sha256_fixture_are_normalized_and_deterministic() {
    let client = LocalEmbeddingClient::default();
    let vectors = client
        .embed(
            vec!["hello".into(), "HELLO hello".into(), "hello world".into()],
            Cancellation::default(),
        )
        .await
        .unwrap();
    // SHA256(hello) begins 2cf24dba5f; bucket 186, positive sign.
    assert_eq!(vectors[0].len(), 256);
    assert_eq!(vectors[0][186], 1.0);
    assert_eq!(vectors[0].iter().filter(|v| **v != 0.0).count(), 1);
    assert_eq!(vectors[0], vectors[1]);
    // SHA256(world) begins 486ea46224; bucket 98, negative sign.
    assert_eq!(vectors[2][186], 1.0 / 2.0_f64.sqrt());
    assert_eq!(vectors[2][98], -1.0 / 2.0_f64.sqrt());
    assert!((vectors[2].iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-12);
    assert!(LocalEmbeddingClient::new(0).is_err());
    let cancellation = Cancellation::default();
    assert!(
        client
            .embed(vec![], cancellation.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(client.embed(vec![" ".into()], cancellation).await.is_err());
}
