mod evaluation_fixtures;
use evaluation_fixtures::*;
use research_backend::{
    evaluations::{EvaluationArtifact, NativeEvaluationService},
    protocol::MAX_FRAME_BYTES,
    providers::ProviderError,
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncWriteExt, sync::Notify};

fn unchanged(
    paths: &research_backend::config::StoragePaths,
    prior: &(std::path::PathBuf, Vec<u8>),
) {
    assert_eq!(fs::read(&prior.0).unwrap(), prior.1);
    assert_eq!(
        fs::read_dir(&paths.evaluation_results).unwrap().count(),
        1,
        "orphan staging file"
    );
}

#[tokio::test]
async fn sequential_completed_runs_are_persisted_before_terminal_and_retain_prior() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let prior = seed(&paths).await;
    for question in ["second café", "third"] {
        let before = artifact_paths(&paths);
        let (_, _, pipeline, _) = rag::fixtures(&[]);
        let mut runtime = Runtime::new(
            NativeEvaluationService::new(pipeline, Arc::new(Judge::default()), &paths).unwrap(),
        );
        runtime.request("eval", vec![case(question)]).await;
        let frames = runtime.terminal("eval").await;
        assert_eq!(frames.last().unwrap()["type"], "result");
        let after = artifact_paths(&paths);
        assert_eq!(after.len(), before.len() + 1);
        let added = after.iter().find(|p| !before.contains(p)).unwrap();
        let result = EvaluationArtifact::parse(&fs::read_to_string(added).unwrap()).unwrap();
        assert_eq!(result.input_cases[0].question, question);
        assert_eq!(fs::read(&prior.0).unwrap(), prior.1);
        runtime.finish().await;
    }
}

// Actual runtime cancel must tear down the paid-stage future before terminal,
// never convert a request cancellation into an isolated failure or call case 2.
#[tokio::test]
async fn cancel_each_stage_drops_work_and_preserves_prior_artifact() {
    for (kind, index) in [
        ("model", 0),
        ("retrieval", 0),
        ("model", 1),
        ("model", 3),
        ("retrieval", 2),
        ("model", 4),
        ("model", 5),
        ("judge", 0),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let prior = seed(&paths).await;
        let (model, retriever, pipeline, _) = rag::fixtures(&["gamma"]);
        let judge = Arc::new(Judge::default());
        let ready = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let behavior = rag::Behavior::Pending(ready.clone(), dropped.clone());
        match kind {
            "model" => {
                model.behavior.lock().unwrap().insert(index, behavior);
            }
            "retrieval" => {
                retriever.behavior.lock().unwrap().insert(index, behavior);
            }
            _ => {
                judge.behavior.lock().unwrap().insert(index, behavior);
            }
        }
        let mut runtime =
            Runtime::new(NativeEvaluationService::new(pipeline, judge.clone(), &paths).unwrap());
        runtime
            .request("eval", vec![case("first"), case("never run")])
            .await;
        runtime.ready(&ready).await;
        let before = model.count.load(Ordering::SeqCst);
        runtime
            .send(json!({"v":1,"type":"cancel","id":"eval"}))
            .await;
        let frames = runtime.terminal("eval").await;
        assert_eq!(
            frames.last().unwrap()["type"],
            "cancelled",
            "{kind}/{index}"
        );
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(model.count.load(Ordering::SeqCst), before);
        assert!(
            !frames
                .iter()
                .any(|v| matches!(v["event"].as_str(), Some("case_failure" | "case_success")))
        );
        unchanged(&paths, &prior);
        runtime.finish().await;
    }
}
#[tokio::test]
async fn self_signalled_cancellation_and_oversize_errors_abort_without_continuing() {
    for behavior in [
        rag::Behavior::Cancel,
        rag::Behavior::Fail(ProviderError::Cancelled),
        rag::Behavior::Fail(ProviderError::TooLarge),
        rag::Behavior::Text("x".repeat(MAX_FRAME_BYTES + 1)),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let prior = seed(&paths).await;
        let (model, _, pipeline, _) = rag::fixtures(&[]);
        let judge = Arc::new(Judge::default());
        judge.behavior.lock().unwrap().insert(0, behavior.clone());
        let mut runtime =
            Runtime::new(NativeEvaluationService::new(pipeline, judge.clone(), &paths).unwrap());
        runtime
            .request("eval", vec![case("first"), case("never run")])
            .await;
        let frames = runtime.terminal("eval").await;
        let last = frames.last().unwrap();
        if matches!(
            behavior,
            rag::Behavior::Cancel | rag::Behavior::Fail(ProviderError::Cancelled)
        ) {
            assert_eq!(last["type"], "cancelled", "{last}");
        } else {
            assert_eq!(last["error"]["code"], "response_too_large");
        }
        assert_eq!(judge.count.load(Ordering::SeqCst), 1);
        assert_eq!(model.count.load(Ordering::SeqCst), 5);
        unchanged(&paths, &prior);
        runtime.finish().await;
    }
}
#[tokio::test]
async fn oversized_case_event_aborts_before_later_cases_or_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let prior = seed(&paths).await;
    let (model, _, pipeline, _) = rag::fixtures(&[]);
    model
        .behavior
        .lock()
        .unwrap()
        .insert(4, rag::Behavior::Text("answer ".repeat(60_000)));
    let judge = Arc::new(Judge::default());
    let mut payload = scores(4);
    payload["context_precision"]["reasoning"] = json!("evidence ".repeat(85_000));
    judge
        .behavior
        .lock()
        .unwrap()
        .insert(0, rag::Behavior::Text(payload.to_string()));
    let mut runtime =
        Runtime::new(NativeEvaluationService::new(pipeline, judge.clone(), &paths).unwrap());
    runtime
        .request("eval", vec![case("first"), case("never run")])
        .await;
    let frames = runtime.terminal("eval").await;
    assert_eq!(
        frames.last().unwrap()["error"]["code"],
        "response_too_large"
    );
    assert_eq!(judge.count.load(Ordering::SeqCst), 1);
    assert_eq!(model.count.load(Ordering::SeqCst), 5);
    unchanged(&paths, &prior);
    runtime.finish().await;
}
#[tokio::test]
async fn validate_all_inputs_before_progress_or_calls_and_reject_concurrent_run_before_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let prior = seed(&paths).await;
    let (model, _, pipeline, _) = rag::fixtures(&[]);
    let judge = Arc::new(Judge::default());
    let service = NativeEvaluationService::new(pipeline, judge, &paths).unwrap();
    let mut runtime = Runtime::new(service);
    let mut bad = case("invalid");
    bad.grading_notes.clear();
    runtime.request("invalid", vec![case("valid"), bad]).await;
    let frames = runtime.terminal("invalid").await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["error"]["code"], "invalid_params");
    assert_eq!(model.count.load(Ordering::SeqCst), 0);
    let ready = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    model
        .behavior
        .lock()
        .unwrap()
        .insert(0, rag::Behavior::Pending(ready.clone(), dropped.clone()));
    runtime.request("first", vec![case("hold lock")]).await;
    runtime.ready(&ready).await;
    runtime.request("second", vec![case("must not run")]).await;
    let frames = runtime.terminal("second").await;
    assert_eq!(frames.last().unwrap()["error"]["code"], "busy");
    assert_eq!(model.count.load(Ordering::SeqCst), 1);
    runtime
        .send(json!({"v":1,"id":"first","type":"cancel"}))
        .await;
    assert_eq!(
        runtime.terminal("first").await.last().unwrap()["type"],
        "cancelled"
    );
    assert!(dropped.load(Ordering::SeqCst));
    unchanged(&paths, &prior);
    runtime.finish().await;
}
#[tokio::test]
async fn eof_and_lost_output_drop_pending_judge_without_artifact_or_worker() {
    for lost_output in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let prior = seed(&paths).await;
        let (_, _, pipeline, _) = rag::fixtures(&[]);
        let judge = Arc::new(Judge::default());
        let ready = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        judge
            .behavior
            .lock()
            .unwrap()
            .insert(0, rag::Behavior::Pending(ready.clone(), dropped.clone()));
        let mut runtime =
            Runtime::new(NativeEvaluationService::new(pipeline, judge, &paths).unwrap());
        runtime.request("eval", vec![case("first")]).await;
        runtime.ready(&ready).await;
        if lost_output {
            let Runtime {
                mut input,
                output,
                task,
            } = runtime;
            drop(output);
            input.shutdown().await.unwrap();
            drop(input);
            let _outcome = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
        } else {
            runtime.input.shutdown().await.unwrap();
            assert_eq!(
                runtime.terminal("eval").await.last().unwrap()["type"],
                "cancelled"
            );
            runtime.finish().await;
        }
        assert!(dropped.load(Ordering::SeqCst));
        unchanged(&paths, &prior);
    }
}
#[tokio::test]
async fn stalled_output_with_full_progress_queue_aborts_incomplete_run() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let prior = seed(&paths).await;
    let (_, _, pipeline, _) = rag::fixtures(&[]);
    let judge = Arc::new(Judge::default());
    let mut runtime = Runtime::with_capacity(
        NativeEvaluationService::new(pipeline, judge.clone(), &paths).unwrap(),
        128,
    );
    runtime.request("eval", vec![case("case"); 40]).await;
    // Runtime's bounded write timeout detects the stalled peer. Do not drain output.
    let outcome = tokio::time::timeout(Duration::from_secs(8), &mut runtime.task)
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.is_err());
    assert!(judge.count.load(Ordering::SeqCst) < 40);
    unchanged(&paths, &prior);
}
#[tokio::test]
async fn completed_failure_reports_are_authoritative_but_no_nested_terminal_is_emitted() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let (_, _, pipeline, _) = rag::fixtures(&[]);
    let judge = Arc::new(Judge::default());
    judge
        .behavior
        .lock()
        .unwrap()
        .insert(0, rag::Behavior::Text("PRIVATE_UPSTREAM_NOT_JSON".into()));
    let mut runtime = Runtime::new(NativeEvaluationService::new(pipeline, judge, &paths).unwrap());
    runtime.request("eval", vec![case("first")]).await;
    let frames = runtime.terminal("eval").await;
    node_validate(&frames);
    assert_eq!(frames.iter().filter(|v| v["type"] != "event").count(), 1);
    assert!(
        !json!(frames)
            .to_string()
            .contains("PRIVATE_UPSTREAM_NOT_JSON")
    );
    assert_eq!(frames.last().unwrap()["data"]["failed"], 1);
    assert_eq!(
        artifact(&paths).results[0].error.as_ref().unwrap().stage,
        "judge"
    );
    assert!(
        frames.last().unwrap()["data"]["aggregate_scores"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    runtime.finish().await;
}
