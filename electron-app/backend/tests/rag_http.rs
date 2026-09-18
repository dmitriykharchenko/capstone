mod provider_support;
mod rag_fixtures;

use provider_support::{FakeHttp, Plan, completion, delta};
use rag_fixtures::*;
use research_backend::{
    providers::{HttpModelClient, ModelConfig, ProviderError},
    rag::{RagErrorKind, RagStage},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn replies() -> Vec<String> {
    vec![
        json!({"subqueries":["alpha","beta"]}).to_string(),
        "summary alpha".into(),
        "summary beta".into(),
        json!({"complete":false,"follow_up_subqueries":["gamma"]}).to_string(),
        "summary gamma".into(),
        "final answer".into(),
    ]
}

fn private_reply(text: &str) -> Plan {
    let mut response = completion(text);
    response["choices"][0]["message"]["reasoning"] = json!("PRIVATE_REASONING");
    response["choices"][0]["message"]["reasoning_details"] = json!([{"text":"PRIVATE_DETAIL"}]);
    Plan::json(response)
}

fn final_sse() -> Plan {
    Plan::sse(
        format!(
            "{}{}{}{}data: [DONE]\n\n",
            delta(json!({"reasoning":"PRIVATE_REASONING"}), Value::Null),
            delta(json!({"content":"final "}), Value::Null),
            delta(json!({"content":"answer"}), Value::Null),
            delta(json!({}), json!("stop"))
        ),
        1,
    )
}

fn pipeline(url: &str) -> research_backend::rag::AdaptiveRag {
    let model = HttpModelClient::new(ModelConfig::new(
        "fake-model".into(),
        url.into(),
        "offline-test".into(),
    ))
    .unwrap();
    build(
        Arc::new(model),
        Arc::new(FakeRetriever::new(Calls::default())),
    )
}

#[tokio::test]
async fn typed_http_adapter_uses_nonstream_private_stages_and_only_public_final_sse() {
    let mut plans = replies()
        .iter()
        .take(5)
        .map(|s| private_reply(s))
        .collect::<Vec<_>>();
    plans.push(final_sse());
    let mut http = FakeHttp::new(plans).await;
    let mut runtime = Runtime::new(pipeline(&http.url));
    runtime.request(false).await;
    let output = runtime.terminal().await;
    assert_eq!(output.last().unwrap()["type"], "result");
    let run = runtime.outcomes.recv().await.unwrap().unwrap();
    assert_eq!(run.hops.len(), 3);
    assert_eq!(run.result.reply, "final answer");
    assert_eq!(
        output
            .iter()
            .filter_map(|v| v["data"]["delta"].as_str())
            .collect::<String>(),
        run.result.reply
    );
    assert!(!json!(output).to_string().contains("PRIVATE"));
    for index in 0..6 {
        let request = http.request().await;
        assert_eq!(request.body["stream"], index == 5);
        assert!(
            request.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.as_object().unwrap().len() == 2)
        );
        if index == 5 {
            let messages = request.body["messages"].as_array().unwrap();
            let evidence: Value =
                serde_json::from_str(messages.last().unwrap()["content"].as_str().unwrap())
                    .unwrap();
            assert_eq!(evidence["subanswers"].as_array().unwrap().len(), 3);
        }
    }
    assert_eq!(http.remaining_requests(), 0);
    assert!(runtime.finish().await.is_empty());
}

#[tokio::test]
async fn typed_http_silent_evaluations_do_not_stream_or_publish_nested_progress() {
    let mut http = FakeHttp::new(replies().iter().map(|s| private_reply(s)).collect()).await;
    let mut runtime = Runtime::new(pipeline(&http.url));
    runtime.request(true).await;
    let output = runtime.terminal().await;
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "result");
    assert_eq!(output[0]["data"]["reply"], "final answer");
    for _ in 0..6 {
        assert_eq!(http.request().await.body["stream"], false);
    }
    assert!(runtime.finish().await.is_empty());
}

#[tokio::test]
async fn actual_http_output_limit_failure_retains_every_stage_including_followup_and_composer() {
    for silent in [false, true] {
        for (index, stage) in [
            (0, RagStage::Planner),
            (1, RagStage::Subanswer),
            (3, RagStage::Coverage),
            (4, RagStage::FollowupSubanswer),
            (5, RagStage::Composer),
        ] {
            let mut plans = replies()
                .iter()
                .take(index)
                .map(|s| private_reply(s))
                .collect::<Vec<_>>();
            if index == 5 && !silent {
                plans.push(Plan::sse(
                    format!(
                        "{}{}data: [DONE]\n\n",
                        delta(json!({"content":"provisional"}), Value::Null),
                        delta(json!({"content":"MUST_NOT_EMIT"}), json!("length"))
                    ),
                    2,
                ));
            } else {
                let mut response = completion("MUST_NOT_EMIT");
                response["choices"][0]["finish_reason"] = json!("length");
                plans.push(Plan::json(response));
            }
            let mut http = FakeHttp::new(plans).await;
            let mut runtime = Runtime::new(pipeline(&http.url));
            runtime.request(silent).await;
            let output = runtime.terminal().await;
            assert_eq!(output.last().unwrap()["type"], "error");
            assert!(!json!(output).to_string().contains("MUST_NOT_EMIT"));
            assert!(!json!(output).to_string().contains("PRIVATE"));
            let error = runtime.outcomes.recv().await.unwrap().unwrap_err();
            assert_eq!(
                (error.stage, error.kind),
                (
                    stage,
                    RagErrorKind::Provider(ProviderError::IncompleteResponse)
                )
            );
            for call in 0..=index {
                assert_eq!(http.request().await.body["stream"], call == 5 && !silent);
            }
            assert_eq!(http.remaining_requests(), 0);
            assert!(runtime.finish().await.is_empty());
        }
    }
}

#[tokio::test]
async fn final_http_socket_close_after_content_cannot_become_success() {
    let mut plans = replies()
        .iter()
        .take(5)
        .map(|s| private_reply(s))
        .collect::<Vec<_>>();
    plans.push(Plan::sse(
        delta(json!({"content":"partial"}), Value::Null),
        3,
    ));
    let http = FakeHttp::new(plans).await;
    let mut runtime = Runtime::new(pipeline(&http.url));
    runtime.request(false).await;
    let output = runtime.terminal().await;
    assert_eq!(output.last().unwrap()["type"], "error");
    assert_eq!(
        runtime.outcomes.recv().await.unwrap().unwrap_err().kind,
        RagErrorKind::Provider(ProviderError::IncompleteResponse)
    );
    assert!(runtime.finish().await.is_empty());
}
