mod evaluation_fixtures;
use evaluation_fixtures::*;
use research_backend::{
    evaluations::{
        EvaluationArtifact, EvaluationRequest, JudgeTemplate, NativeEvaluationService,
        load_default_cases, parse_judge_output, parse_test_cases,
    },
    protocol::{ErrorCode, MAX_FRAME_BYTES},
    providers::ProviderError,
    rag::RunMode,
    runtime::Cancellation,
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, atomic::Ordering},
};

#[test]
fn strict_entire_input_array_and_hybrid_only_strategy() {
    for raw in [
        "null",
        "{}",
        "[]",
        "[1]",
        r#"[["q","n",[]]]"#,
        r#"[{"question":"q","grading_notes":"n"}]"#,
        r#"[{"question":"q","question":"q","grading_notes":"n","sources":[]}]"#,
        r#"[{"question":"q","grading_notes":"n","sources":[],"extra":1}]"#,
        r#"[{"question":"q","grading_notes":"n","sources":[null]}]"#,
        r#"[{"question":"q","grading_notes":"n","sources":{}}]"#,
    ] {
        assert!(parse_test_cases(raw).is_err(), "{raw}");
    }
    for field in ["question", "grading_notes"] {
        for bad in [
            json!(""),
            json!(" \n\u{feff}"),
            json!(false),
            json!([]),
            Value::Null,
        ] {
            let mut cases = json!([case("valid"), case("invalid")]);
            cases[1][field] = bad;
            assert!(parse_test_cases(&cases.to_string()).is_err());
        }
    }
    for strategy in ["vector", "bm25", "multihop-hybrid", ""] {
        assert!(
            EvaluationRequest::from_value(
                json!({"retrieval_type":strategy,"test_cases":[case("q")]})
            )
            .is_err()
        );
    }
    assert_eq!(load_default_cases().unwrap().len(), 46);
    assert!(parse_test_cases(&json!(vec![case("q"); 1001]).to_string()).is_err());
    assert_eq!(
        parse_test_cases(&" ".repeat(MAX_FRAME_BYTES + 1)).unwrap_err(),
        ErrorCode::ResponseTooLarge
    );
}
#[test]
fn judge_schema_duplicate_keys_arrays_nonfinite_unknowns_and_bounds() {
    for score in 0..=5 {
        assert_eq!(
            parse_judge_output(&scores(score).to_string())
                .unwrap()
                .scores(),
            [score; 7]
        );
    }
    for raw in [
        "null",
        "[]",
        "{}",
        "[[]]",
        "not json",
        "{\"score\":NaN}",
        "{\"score\":Infinity}",
        "{\"score\":1e999}",
    ] {
        assert!(parse_judge_output(raw).is_err());
    }
    let valid = scores(4).to_string();
    assert!(
        parse_judge_output(&valid.replacen("\"score\":4", "\"score\":4,\"score\":4", 1)).is_err()
    );
    assert!(
        parse_judge_output(&format!(
            "{{\"completeness\":{},{}",
            scores(4)["completeness"],
            &valid[1..]
        ))
        .is_err()
    );
    for (field, bad) in [
        ("score", json!(true)),
        ("score", json!(4.0)),
        ("score", json!(-1)),
        ("score", json!(6)),
        ("confidence", json!("certain")),
        ("reasoning", json!(" ")),
        ("flags", json!([""])),
        ("flags", json!({})),
        ("flags", json!([3])),
    ] {
        let mut v = scores(4);
        v["context_precision"][field] = bad;
        assert!(parse_judge_output(&v.to_string()).is_err(), "{field}");
    }
    for name in research_backend::evaluations::JUDGE_CRITERIA {
        let mut v = scores(4);
        v.as_object_mut().unwrap().remove(name);
        assert!(parse_judge_output(&v.to_string()).is_err());
        let mut v = scores(4);
        v[name]["private_reasoning"] = json!("NO");
        assert!(parse_judge_output(&v.to_string()).is_err());
        let mut v = scores(4);
        v[name] = json!([4, "high", "reason", []]);
        assert!(parse_judge_output(&v.to_string()).is_err());
    }
    for bad in [
        json!([0]),
        json!([36]),
        json!([true]),
        json!([1.0]),
        json!(["1"]),
        json!(vec![1; 36]),
    ] {
        let mut v = scores(4);
        v["relevant_chunk_ranking"]["relevant_chunk_ranks"] = bad;
        assert!(parse_judge_output(&v.to_string()).is_err());
    }
    for bad in [
        json!([0]),
        json!([36]),
        json!([true]),
        json!([1.0]),
        json!([""]),
        json!([{}]),
    ] {
        let mut v = scores(4);
        v["context_utilization"]["chunks_used"] = bad;
        assert!(parse_judge_output(&v.to_string()).is_err());
    }
}
#[test]
fn template_exact_mapping_ranked_full_text_and_no_recursive_substitution() {
    let mut input = case("{{generated_answer}} literal question");
    let chunks = vec![
        rag::chunk(1, "alpha", 0).into(),
        rag::chunk(2, "beta", 0).into(),
    ];
    let template = JudgeTemplate::embedded().unwrap();
    let messages = template.render(&input, &chunks, "answer").unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[1].role, "user");
    let text = &messages[1].content;
    assert!(text.contains("USER_QUERY: {{generated_answer}} literal question"));
    assert!(text.contains("1. evidence from alpha\n2. evidence from beta"));
    assert!(text.contains("GROUND_TRUTH_RELEVANT: [\"alpha.pdf\", \"beta.pdf\"]"));
    assert!(text.contains("GENERATED_ANSWER: answer"));
    assert!(text.contains("REFERENCE_ANSWER: Grounded reference — café"));
    assert!(!text.contains("STALE_SNIPPET"));
    input.sources.clear();
    assert!(
        template.render(&input, &chunks, "answer").unwrap()[1]
            .content
            .contains("GROUND_TRUTH_RELEVANT: NOT PROVIDED")
    );
    assert!(JudgeTemplate::parse("SYSTEM PROMPT:\n\nUSER PROMPT (fill per example):\n").is_err());
}

#[tokio::test]
async fn zero_one_two_followups_share_core_evidence_and_emit_only_ordered_evaluation_events() {
    for followups in [vec![], vec!["gamma"], vec!["gamma", "delta"]] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let (_, _, pipeline, calls) = rag::fixtures(&followups);
        let judge = Arc::new(Judge::default());
        let service = NativeEvaluationService::new(pipeline, judge.clone(), &paths).unwrap();
        fs::write(&paths.history_database, "untouched history sentinel").unwrap();
        let mut runtime = Runtime::new(service);
        runtime.request("eval", vec![case(rag::QUESTION)]).await;
        let frames = runtime.terminal("eval").await;
        node_validate(&frames);
        assert_eq!(
            frames
                .iter()
                .filter_map(|f| f["event"].as_str())
                .collect::<Vec<_>>(),
            ["run_start", "case_start", "case_success"]
        );
        assert_eq!(frames.last().unwrap()["type"], "result");
        for (seq, f) in frames.iter().enumerate() {
            assert_eq!(f["seq"], seq);
            assert!(!f.to_string().contains("PRIVATE_SYSTEM_MARKER"));
        }
        let artifact = artifact(&paths);
        assert_eq!(artifact.results.len(), 1);
        let (_, _, chat, chat_calls) = rag::fixtures(&followups);
        let chat = chat
            .run_stateless(
                rag::QUESTION.into(),
                vec![],
                RunMode::Silent(Cancellation::default()),
            )
            .await
            .unwrap();
        assert_eq!(
            artifact.results[0].generated_answer.as_deref(),
            Some(chat.result.reply.as_str())
        );
        assert_eq!(
            json!(artifact.results[0].retrieved_chunks),
            json!(
                chat.result
                    .chunks
                    .into_iter()
                    .map(research_backend::evaluations::RetrievedChunk::from)
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            format!("{:?}", calls.lock().unwrap()),
            format!("{:?}", chat_calls.lock().unwrap())
        );
        assert_eq!(
            fs::read_to_string(&paths.history_database).unwrap(),
            "untouched history sentinel"
        );
        assert_eq!(fs::read_dir(&paths.data_root).unwrap().count(), 2);
        assert_eq!(judge.count.load(Ordering::SeqCst), 1);
        runtime.finish().await;
    }
}

#[tokio::test]
async fn case_failures_keep_stages_answers_and_continue_in_order_with_partial_and_null_aggregates()
{
    for (index, stage) in [
        (0, "planner"),
        (1, "subanswer"),
        (3, "coverage"),
        (4, "followup_subanswer"),
        (5, "composer"),
        (6, "judge"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let (model, _, pipeline, _) = rag::fixtures(&["gamma"]);
        let judge = Arc::new(Judge::default());
        if stage == "judge" {
            judge
                .behavior
                .lock()
                .unwrap()
                .insert(0, rag::Behavior::Fail(ProviderError::IncompleteResponse));
        } else {
            model.behavior.lock().unwrap().insert(
                index,
                rag::Behavior::Fail(ProviderError::IncompleteResponse),
            );
        }
        let mut runtime =
            Runtime::new(NativeEvaluationService::new(pipeline, judge, &paths).unwrap());
        runtime
            .request("eval", vec![case("first"), case("second")])
            .await;
        let frames = runtime.terminal("eval").await;
        node_validate(&frames);
        assert_eq!(frames.last().unwrap()["data"]["succeeded"], 1, "{stage}");
        let artifact = artifact(&paths);
        let first = &artifact.results[0];
        assert_eq!(first.error.as_ref().unwrap().stage, stage);
        assert!(first.error.as_ref().unwrap().message.contains("incomplete"));
        assert_eq!(first.generated_answer.is_some(), stage == "judge");
        assert!(artifact.results[1].error.is_none());
        assert_eq!(
            frames.last().unwrap()["data"]["aggregate_scores"]["context_precision"],
            4.0
        );
        runtime.finish().await;
    }
    for (index, stage) in [(0, "retrieval"), (2, "followup_retrieval")] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let (_, retriever, pipeline, _) = rag::fixtures(&["gamma"]);
        retriever
            .behavior
            .lock()
            .unwrap()
            .insert(index, rag::Behavior::Fail(ProviderError::Transport));
        let mut runtime = Runtime::new(
            NativeEvaluationService::new(pipeline, Arc::new(Judge::default()), &paths).unwrap(),
        );
        runtime
            .request("eval", vec![case("first"), case("second")])
            .await;
        let frames = runtime.terminal("eval").await;
        node_validate(&frames);
        assert_eq!(
            artifact(&paths).results[0].error.as_ref().unwrap().stage,
            stage
        );
        assert_eq!(frames.last().unwrap()["data"]["succeeded"], 1);
        runtime.finish().await;
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let (_, _, pipeline, _) = rag::fixtures(&[]);
    let judge = Arc::new(Judge::default());
    for i in 0..2 {
        judge
            .behavior
            .lock()
            .unwrap()
            .insert(i, rag::Behavior::Text("malformed".into()));
    }
    let mut runtime = Runtime::new(NativeEvaluationService::new(pipeline, judge, &paths).unwrap());
    runtime
        .request("eval", vec![case("first"), case("second")])
        .await;
    let frames = runtime.terminal("eval").await;
    node_validate(&frames);
    assert_eq!(frames.last().unwrap()["data"]["failed"], 2);
    assert!(
        frames.last().unwrap()["data"]["aggregate_scores"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    runtime.finish().await;
}

#[tokio::test]
async fn unrounded_aggregates_and_strict_artifact_relationship_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let (_, _, pipeline, _) = rag::fixtures(&[]);
    let judge = Arc::new(Judge::default());
    for (i, score) in [0, 1, 1].into_iter().enumerate() {
        judge
            .behavior
            .lock()
            .unwrap()
            .insert(i, rag::Behavior::Text(scores(score).to_string()));
    }
    let mut runtime = Runtime::new(NativeEvaluationService::new(pipeline, judge, &paths).unwrap());
    runtime
        .request("eval", vec![case("a"), case("b"), case("c")])
        .await;
    let frames = runtime.terminal("eval").await;
    node_validate(&frames);
    assert_eq!(
        frames.last().unwrap()["data"]["aggregate_scores"]["context_precision"],
        2.0 / 3.0
    );
    let value = json!(artifact(&paths));
    for (pointer, bad) in [
        ("/results/0/case_index", json!(0)),
        ("/results/0/input/question", json!("other")),
        ("/results/0/retrieved_chunks/0/rank", json!(2)),
        ("/results/0/retrieved_chunks/0/page", json!(0)),
        ("/schema_version", json!(2)),
        ("/summary/total", json!(9)),
        ("/aggregate_scores/context_precision", Value::Null),
    ] {
        let mut v = value.clone();
        *v.pointer_mut(pointer).unwrap() = bad;
        assert!(
            EvaluationArtifact::parse(&v.to_string()).is_err(),
            "{pointer}"
        );
    }
    runtime.finish().await;
}
