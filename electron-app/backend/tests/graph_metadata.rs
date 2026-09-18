mod provider_support;
use provider_support::{FakeHttp, Plan, completion};
use research_backend::{
    domain::Message,
    graph::*,
    protocol::ErrorCode,
    providers::{HttpModelClient, ModelClient, ModelConfig},
    registry::BackendFuture,
    runtime::{Cancellation, RequestContext},
};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct FakeModel {
    response: Result<String, ErrorCode>,
    calls: Mutex<Vec<Vec<Message>>>,
}
impl ModelClient for FakeModel {
    fn complete(&self, messages: Vec<Message>, _: Cancellation) -> BackendFuture<String> {
        self.calls.lock().unwrap().push(messages);
        let response = self.response.clone();
        Box::pin(async move { response })
    }
    fn stream(&self, _: Vec<Message>, _: RequestContext) -> BackendFuture<String> {
        panic!("private metadata must never stream")
    }
}
fn payload() -> String {
    json!({"title":"  A   Useful Paper ","authors":[" Alice  Smith ","alice smith","Bob Jones"],"topics":[" Retrieval ","RAG","machine   learning"]}).to_string()
}

#[tokio::test]
async fn private_bounded_extraction_preserves_prompt_order_and_unicode_char_limit() {
    let model = Arc::new(FakeModel {
        response: Ok(payload()),
        calls: Mutex::new(vec![]),
    });
    let extractor = PaperMetadataExtractor::new(model.clone());
    let metadata = extractor
        .extract_metadata(
            &format!("  first   line\r\n\n{}", "界".repeat(20_000)),
            Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(metadata.title(), "A Useful Paper");
    assert_eq!(metadata.authors(), ["Alice Smith", "Bob Jones"]);
    assert_eq!(metadata.topics(), ["retrieval", "rag", "machine learning"]);
    let calls = model.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][0].role, "system");
    assert_eq!(calls[0][0].content, METADATA_SYSTEM_PROMPT);
    assert_eq!(calls[0][1].role, "user");
    let evidence = calls[0][1]
        .content
        .split("<paper_text>\n")
        .nth(1)
        .unwrap()
        .split("\n</paper_text>")
        .next()
        .unwrap();
    assert_eq!(evidence.chars().count(), MAX_METADATA_TEXT_CHARS);
    assert!(evidence.starts_with("first line\n"));
}

#[test]
fn metadata_dedup_blanks_and_full_fold_preserve_first_seen_order() {
    let metadata = parse_metadata_output(&json!({"title":" X ","authors":["Zed"," anna  lee ","ZED"," ","Bob","Straße","STRASSE"],"topics":["Topic C","topic a","TOPIC C","\n","Topic B"]}).to_string()).unwrap();
    assert_eq!(metadata.authors(), ["Zed", "anna lee", "Bob", "Straße"]);
    assert_eq!(metadata.topics(), ["topic c", "topic a", "topic b"]);
}

#[test]
fn strict_output_rejects_malformed_duplicate_extra_nonstring_and_range_errors() {
    assert_eq!(
        parse_metadata_output(r#"["x",["a"],["a","b","c"]]"#),
        Err(MetadataError::Output)
    );
    for text in [
        "",
        "not json",
        "null",
        "[]",
        "{}",
        "{\"title\":\"x\",\"authors\":[\"a\"],\"topics\":[\"a\",\"b\",\"c\"]",
        "{\"title\":\"x\",\"authors\":[\"a\"],\"topics\":[\"a\",\"b\",NaN]}",
        "{\"title\":\"x\",\"title\":\"y\",\"authors\":[\"a\"],\"topics\":[\"a\",\"b\",\"c\"]}",
    ] {
        assert_eq!(parse_metadata_output(text), Err(MetadataError::Output));
    }
    for (key, value) in [
        ("extra", json!(1)),
        ("title", json!(" ")),
        ("authors", json!([])),
        ("authors", json!([" "])),
        ("authors", json!([1])),
        ("topics", json!(["a", "b"])),
        ("topics", json!(["a", "A", "b"])),
        (
            "topics",
            json!((0..11).map(|n| n.to_string()).collect::<Vec<_>>()),
        ),
        ("topics", json!("abc")),
    ] {
        let mut raw: serde_json::Value = serde_json::from_str(&payload()).unwrap();
        raw[key] = value;
        assert_eq!(
            parse_metadata_output(&raw.to_string()),
            Err(MetadataError::Output)
        );
    }
}

#[tokio::test]
async fn empty_input_cancel_and_model_failure_are_explicit_and_sanitized() {
    let model = Arc::new(FakeModel {
        response: Err(ErrorCode::Internal),
        calls: Mutex::new(vec![]),
    });
    let extractor = PaperMetadataExtractor::new(model.clone());
    for text in ["", " \n\t "] {
        assert_eq!(
            extractor
                .extract_metadata(text, Cancellation::default())
                .await,
            Err(MetadataError::InvalidInput)
        );
    }
    let c = Cancellation::default();
    c.cancel();
    assert_eq!(
        extractor.extract_metadata("text", c).await,
        Err(MetadataError::Cancelled)
    );
    assert!(model.calls.lock().unwrap().is_empty());
    assert_eq!(
        extractor
            .extract_metadata("text", Cancellation::default())
            .await,
        Err(MetadataError::Model)
    );
    assert_eq!(
        extractor
            .extract("text".into(), Cancellation::default())
            .await,
        Err(ErrorCode::Internal)
    );
}

#[tokio::test]
async fn typed_http_completion_is_private_json_and_detects_incomplete_output() {
    let mut incomplete = completion(&payload());
    incomplete["choices"][0]["finish_reason"] = json!("length");
    let mut server = FakeHttp::new(vec![
        Plan::json(completion(&payload())),
        Plan::json(incomplete),
        Plan::json(completion("private malformed text")),
    ])
    .await;
    let model = HttpModelClient::new(ModelConfig::new(
        "fake-metadata".into(),
        server.url.clone(),
        "offline-test".into(),
    ))
    .unwrap();
    let extractor = PaperMetadataExtractor::from_http(model);
    assert!(
        extractor
            .extract_metadata("Evidence", Cancellation::default())
            .await
            .is_ok()
    );
    let request = server.request().await;
    assert_eq!(request.body["stream"], false);
    assert_eq!(
        request.body["response_format"],
        json!({"type":"json_object"})
    );
    assert_eq!(
        extractor
            .extract_metadata("Evidence", Cancellation::default())
            .await,
        Err(MetadataError::Incomplete)
    );
    server.request().await;
    assert_eq!(
        extractor
            .extract_metadata("Evidence", Cancellation::default())
            .await,
        Err(MetadataError::Output)
    );
    server.request().await;
}

#[tokio::test]
async fn cancellation_during_private_completion_discards_metadata() {
    struct CancelModel;
    impl ModelClient for CancelModel {
        fn complete(&self, _: Vec<Message>, c: Cancellation) -> BackendFuture<String> {
            Box::pin(async move {
                c.cancel();
                Ok(payload())
            })
        }
        fn stream(&self, _: Vec<Message>, _: RequestContext) -> BackendFuture<String> {
            panic!("private stage")
        }
    }
    let model = PaperMetadataExtractor::new(Arc::new(CancelModel));
    assert_eq!(
        model
            .extract_metadata("paper", Cancellation::default())
            .await,
        Err(MetadataError::Cancelled)
    );
}

#[tokio::test]
async fn line_normalization_handles_unicode_and_python_information_separators() {
    let model = Arc::new(FakeModel {
        response: Ok(payload()),
        calls: Mutex::new(vec![]),
    });
    PaperMetadataExtractor::new(model.clone())
        .extract_metadata(
            "\n one\u{1f} two \u{1c}\u{85} three\u{2028}\u{2029}\t four \n",
            Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(
        model.calls.lock().unwrap()[0][1]
            .content
            .contains("<paper_text>\none two\nthree\nfour\n</paper_text>")
    );
    let mut ten: serde_json::Value = serde_json::from_str(&payload()).unwrap();
    ten["topics"] = json!((0..10).map(|n| n.to_string()).collect::<Vec<_>>());
    assert_eq!(
        parse_metadata_output(&ten.to_string())
            .unwrap()
            .topics()
            .len(),
        10
    );
}
