mod rag_fixtures;

use rag_fixtures::*;
use research_backend::{
    protocol::MAX_FRAME_BYTES,
    providers::ProviderError,
    rag::{RagErrorKind, RagStage, RunMode},
    runtime::Cancellation,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

// Ported from Python stream_events ordering, adaptive followups, interrupted/
// abandoned stream tests, plus test_evaluation_runner Chat/Evals parity. Uses
// actual serve/RequestContext with test-only registry, never production handlers.
#[tokio::test]
async fn public_event_order_dynamic_totals_and_authoritative_final_reconciliation() {
    for followups in [vec![], vec!["gamma"], vec!["gamma", "delta"]] {
        let (_, _, pipeline, calls) = fixtures(&followups);
        let mut runtime = Runtime::new(pipeline);
        runtime.request(false).await;
        let output = runtime.terminal().await;
        let run = runtime.outcomes.recv().await.unwrap().unwrap();
        let mut names = vec![
            "plan",
            "subquery",
            "subanswer",
            "subquery",
            "subanswer",
            "coverage",
            "coverage",
        ];
        for _ in &followups {
            names.extend(["subquery", "subanswer"]);
        }
        names.extend(["delta", "delta"]);
        assert_eq!(
            output
                .iter()
                .filter_map(|v| v["event"].as_str())
                .collect::<Vec<_>>(),
            names
        );
        assert_eq!(
            output[0]["data"],
            json!({"subqueries":[
                {"id":"hop-1","index":1,"query":"alpha evidence"},
                {"id":"hop-2","index":2,"query":"beta evidence"},
            ]})
        );
        assert_eq!(
            output[5]["data"],
            json!({"status":"reviewing","total":2,"subqueries":[]})
        );
        assert_eq!(
            output[6]["data"]["status"],
            if followups.is_empty() {
                "complete"
            } else {
                "followup"
            }
        );
        assert_eq!(output[6]["data"]["total"], 2 + followups.len());
        for (position, query) in followups.iter().enumerate() {
            let index = position + 3;
            let expected = json!({"id":format!("hop-{index}"),"index":index,"query":query});
            assert_eq!(output[6]["data"]["subqueries"][position], expected);
            let start = &output[7 + position * 2]["data"];
            assert_eq!(start["index"], index);
            assert_eq!(start["total"], 2 + followups.len());
            assert_eq!(start["id"], expected["id"]);
            assert_eq!(output[8 + position * 2]["data"]["sources"][0]["rank"], 1);
        }
        let final_result = output.last().unwrap();
        assert_eq!(final_result["type"], "result");
        assert_eq!(final_result["data"]["reply"], run.result.reply);
        assert_eq!(final_result["data"]["sources"], json!(run.result.sources));
        let deltas: String = output
            .iter()
            .filter_map(|v| v["data"]["delta"].as_str())
            .collect();
        assert_eq!(deltas, run.result.reply);
        for (seq, item) in output.iter().enumerate() {
            assert_eq!(item["seq"], seq);
            assert!(!item.to_string().contains("PRIVATE"));
            assert!(!item.to_string().contains("MULTIHOP_"));
            assert!(item.to_string().len() <= MAX_FRAME_BYTES);
        }
        assert_eq!(output.iter().filter(|v| v["type"] != "event").count(), 1);
        assert!(runtime.finish().await.is_empty());
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(
                    c,
                    Call::Model {
                        streaming: true,
                        ..
                    }
                ))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn silent_evaluations_inside_streaming_context_share_identical_prompts_evidence_and_answer() {
    for followups in [vec![], vec!["gamma"], vec!["gamma", "delta"]] {
        let (_, _, chat, chat_calls) = fixtures(&followups);
        let mut runtime = Runtime::new(chat);
        runtime.request(false).await;
        let chat_output = runtime.terminal().await;
        let chat = runtime.outcomes.recv().await.unwrap().unwrap();
        assert!(runtime.finish().await.is_empty());
        let (_, _, eval, eval_calls) = fixtures(&followups);
        let mut runtime = Runtime::new(eval);
        runtime.request(true).await;
        let eval_output = runtime.terminal().await;
        let eval = runtime.outcomes.recv().await.unwrap().unwrap();
        assert_eq!(
            eval_output.len(),
            1,
            "nested RAG event leaked into evaluate_stream"
        );
        assert_eq!(eval_output[0]["type"], "result");
        assert_eq!(chat_output.last().unwrap()["data"], eval_output[0]["data"]);
        assert_eq!(json!(chat.result), json!(eval.result));
        assert_eq!(
            chat.hops.iter().map(|h| &h.context).collect::<Vec<_>>(),
            eval.hops.iter().map(|h| &h.context).collect::<Vec<_>>()
        );
        assert!(runtime.finish().await.is_empty());
        let chat_calls = chat_calls.lock().unwrap();
        let eval_calls = eval_calls.lock().unwrap();
        assert_eq!(chat_calls.len(), eval_calls.len());
        for (chat, eval) in chat_calls.iter().zip(eval_calls.iter()) {
            match (chat, eval) {
                (
                    Call::Model {
                        stage: a,
                        messages: am,
                        streaming: astream,
                    },
                    Call::Model {
                        stage: b,
                        messages: bm,
                        streaming: bstream,
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(json!(am), json!(bm));
                    assert_eq!(*astream, *a == "composer");
                    assert!(!bstream);
                }
                (
                    Call::Retrieve {
                        query: a,
                        strategy: as_,
                        scope: ac,
                        limit: al,
                    },
                    Call::Retrieve {
                        query: b,
                        strategy: bs,
                        scope: bc,
                        limit: bl,
                    },
                ) => {
                    assert_eq!((a, as_, ac, al), (b, bs, bc, bl));
                }
                _ => panic!("call order differs"),
            }
        }
    }
}

#[tokio::test]
async fn existing_rag_pipeline_trait_returns_result_without_nested_terminals() {
    let (_, _, pipeline, _) = fixtures(&["gamma"]);
    let mut runtime = Runtime::new(pipeline);
    runtime.send(json!({"v":1,"type":"request","id":"rag","command":"chat_stream","params":{"trait":true}})).await;
    let output = runtime.terminal().await;
    assert_eq!(output.last().unwrap()["data"]["reply"], "final answer");
    assert_eq!(output.iter().filter(|v| v["type"] == "result").count(), 1);
    assert!(
        !output
            .iter()
            .any(|v| v["event"] == "done" || v["event"] == "complete")
    );
    assert!(runtime.finish().await.is_empty());
}

#[tokio::test]
async fn each_public_model_stage_failure_has_one_error_and_no_successful_partial_result() {
    for (index, stage) in [
        (0, RagStage::Planner),
        (1, RagStage::Subanswer),
        (3, RagStage::Coverage),
        (4, RagStage::FollowupSubanswer),
        (5, RagStage::Composer),
    ] {
        let (model, _, pipeline, _) = fixtures(&["gamma"]);
        model
            .behavior
            .lock()
            .unwrap()
            .insert(index, Behavior::Fail(ProviderError::IncompleteResponse));
        let mut runtime = Runtime::new(pipeline);
        runtime.request(false).await;
        let output = runtime.terminal().await;
        assert_eq!(output.last().unwrap()["type"], "error");
        assert!(
            !output
                .iter()
                .any(|v| v["event"] == "delta" || v["type"] == "result")
        );
        let error = runtime.outcomes.recv().await.unwrap().unwrap_err();
        assert_eq!(
            (error.stage, error.kind),
            (
                stage,
                RagErrorKind::Provider(ProviderError::IncompleteResponse)
            )
        );
        assert!(runtime.finish().await.is_empty());
    }
}

#[tokio::test]
async fn streamed_partial_deltas_are_provisional_on_truncation_or_transport_failure() {
    for error in [ProviderError::IncompleteResponse, ProviderError::Transport] {
        let calls = Calls::default();
        let mut model = FakeModel::new(calls.clone(), &["alpha", "beta"], &[]);
        model.final_parts = vec!["partial ".into(), "text".into()];
        model.stream_error = Some(error);
        let mut runtime = Runtime::new(build(Arc::new(model), Arc::new(FakeRetriever::new(calls))));
        runtime.request(false).await;
        let output = runtime.terminal().await;
        assert_eq!(
            output
                .iter()
                .filter_map(|v| v["data"]["delta"].as_str())
                .collect::<String>(),
            "partial text"
        );
        assert_eq!(output.last().unwrap()["type"], "error");
        assert_eq!(
            runtime.outcomes.recv().await.unwrap().unwrap_err().kind,
            RagErrorKind::Provider(error)
        );
        assert!(runtime.finish().await.is_empty());
    }
}

async fn until_dropped(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn runtime_cancellation_drops_every_inflight_model_and_retrieval_stage() {
    for retrieval in [false, true] {
        for index in 0..if retrieval { 4 } else { 7 } {
            let (model, retriever, pipeline, _) = fixtures(&["gamma", "delta"]);
            let started = Arc::new(Notify::new());
            let dropped = Arc::new(AtomicBool::new(false));
            let behavior = Behavior::Pending(started.clone(), dropped.clone());
            if retrieval {
                retriever.behavior.lock().unwrap().insert(index, behavior);
            } else {
                model.behavior.lock().unwrap().insert(index, behavior);
            }
            let mut runtime = Runtime::new(pipeline);
            runtime.request(false).await;
            tokio::time::timeout(Duration::from_secs(3), started.notified())
                .await
                .unwrap();
            runtime
                .send(json!({"v":1,"type":"cancel","id":"rag"}))
                .await;
            let output = runtime.terminal().await;
            assert_eq!(output.last().unwrap()["type"], "cancelled");
            assert_eq!(runtime.next().await["accepted"], true);
            assert!(!output.iter().any(|v| v["type"] == "result"));
            until_dropped(&dropped).await;
            assert!(runtime.finish().await.is_empty());
            assert_eq!(
                if retrieval {
                    retriever.count.load(Ordering::SeqCst)
                } else {
                    model.count.load(Ordering::SeqCst)
                },
                index + 1
            );
        }
    }
}

#[tokio::test]
async fn direct_silent_cancellation_interrupts_pending_stage_and_future_drop_stops_work() {
    for cancel in [true, false] {
        let (model, _, pipeline, _) = fixtures(&[]);
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        model
            .behavior
            .lock()
            .unwrap()
            .insert(3, Behavior::Pending(started.clone(), dropped.clone()));
        let token = Cancellation::default();
        let running = token.clone();
        let task = tokio::spawn(async move {
            pipeline
                .run_stateless(QUESTION.into(), vec![], RunMode::Silent(running))
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        if cancel {
            token.cancel();
            let error = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(
                (error.stage, error.kind),
                (RagStage::Coverage, RagErrorKind::Cancelled)
            );
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        until_dropped(&dropped).await;
        assert_eq!(model.count.load(Ordering::SeqCst), 4);
    }
}

#[tokio::test]
async fn closed_request_transport_cancels_pending_work_without_a_result() {
    let (model, _, pipeline, _) = fixtures(&[]);
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    model
        .behavior
        .lock()
        .unwrap()
        .insert(1, Behavior::Pending(started.clone(), dropped.clone()));
    let mut runtime = Runtime::new(pipeline);
    runtime.request(false).await;
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let output = runtime.finish().await;
    assert!(!output.iter().any(|v| v["type"] == "result"));
    assert_eq!(output.last().unwrap()["type"], "cancelled");
    until_dropped(&dropped).await;
    assert_eq!(model.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn oversized_progress_delta_and_aggregate_result_never_succeed() {
    for silent in [false, true] {
        let (model, _, pipeline, _) = fixtures(&[]);
        model
            .behavior
            .lock()
            .unwrap()
            .insert(1, Behavior::Text("x".repeat(MAX_FRAME_BYTES - 600)));
        let mut runtime = Runtime::new(pipeline);
        runtime.request(silent).await;
        let output = runtime.terminal().await;
        assert_eq!(output.last().unwrap()["type"], "error");
        assert_eq!(
            runtime.outcomes.recv().await.unwrap().unwrap_err().kind,
            RagErrorKind::TooLarge
        );
        assert!(runtime.finish().await.is_empty());
    }
    let calls = Calls::default();
    let mut model = FakeModel::new(calls.clone(), &["a", "b"], &[]);
    model.final_parts = vec!["x".repeat(MAX_FRAME_BYTES)];
    let mut runtime = Runtime::new(build(Arc::new(model), Arc::new(FakeRetriever::new(calls))));
    runtime.request(false).await;
    let output = runtime.terminal().await;
    assert_eq!(
        output.last().unwrap()["error"]["code"],
        "response_too_large"
    );
    assert!(!output.iter().any(|v| v["event"] == "delta"));
    assert!(runtime.finish().await.is_empty());
    for silent in [false, true] {
        let calls = Calls::default();
        let model = Arc::new(FakeModel::new(calls.clone(), &["a", "b"], &[]));
        let mut retriever = FakeRetriever::new(calls);
        for query in ["a", "b"] {
            let mut evidence = chunk(1, query, 0);
            evidence.text = "x".repeat(550_000);
            retriever.results.insert(query.into(), vec![evidence]);
        }
        let mut runtime = Runtime::new(build(model, Arc::new(retriever)));
        runtime.request(silent).await;
        let output = runtime.terminal().await;
        assert_eq!(output.last().unwrap()["type"], "error");
        let error = runtime.outcomes.recv().await.unwrap().unwrap_err();
        assert_eq!(
            (error.stage, error.kind),
            (RagStage::Composer, RagErrorKind::TooLarge)
        );
        assert!(runtime.finish().await.is_empty());
    }
}

#[tokio::test]
async fn public_payloads_match_electron_validators_without_browser_tools() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let (_, _, pipeline, _) = fixtures(&["gamma", "delta"]);
    let mut runtime = Runtime::new(pipeline);
    runtime.request(false).await;
    let events = runtime
        .terminal()
        .await
        .into_iter()
        .filter(|v| v["type"] == "event")
        .collect::<Vec<_>>();
    assert!(runtime.finish().await.is_empty());
    // Feed only synthetic public fixtures through existing Electron validators.
    let mut node=Command::new("node").args(["--input-type=module","-e",
        "import {parseEvent} from './electron/adapters.mjs'; let s=''; for await (const c of process.stdin) s+=c; for (const v of JSON.parse(s)) parseEvent('chat_stream',v.event,v.data);"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"),"/.."))
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    node.stdin
        .take()
        .unwrap()
        .write_all(json!(events).to_string().as_bytes())
        .unwrap();
    assert!(node.wait().unwrap().success());
}

#[tokio::test]
async fn cancellation_after_final_deltas_never_promotes_them_to_a_result() {
    let calls = Calls::default();
    let mut model = FakeModel::new(calls.clone(), &["alpha", "beta"], &[]);
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    model.stream_pending = Some((started.clone(), dropped.clone()));
    let mut runtime = Runtime::new(build(Arc::new(model), Arc::new(FakeRetriever::new(calls))));
    runtime.request(false).await;
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let mut events = vec![];
    loop {
        let event = runtime.next().await;
        let last_delta = event["data"]["delta"] == "answer";
        events.push(event);
        if last_delta {
            break;
        }
    }
    runtime
        .send(json!({"v":1,"type":"cancel","id":"rag"}))
        .await;
    assert_eq!(runtime.next().await["type"], "cancelled");
    assert_eq!(runtime.next().await["accepted"], true);
    assert_eq!(
        events
            .iter()
            .filter_map(|v| v["data"]["delta"].as_str())
            .collect::<String>(),
        "final answer"
    );
    assert!(runtime.finish().await.is_empty());
    until_dropped(&dropped).await;
}

#[tokio::test]
async fn malformed_coverage_and_initial_followup_source_errors_emit_no_final_deltas() {
    for failure in 0..3 {
        let (model, retriever, pipeline, _) = fixtures(&["gamma"]);
        match failure {
            0 => {
                model.behavior.lock().unwrap().insert(
                    3,
                    Behavior::Text(
                        r#"{"complete":true,"follow_up_subqueries":[],"reasoning":"PRIVATE"}"#
                            .into(),
                    ),
                );
            }
            1 | 2 => {
                retriever.behavior.lock().unwrap().insert(
                    if failure == 1 { 0 } else { 2 },
                    Behavior::Fail(ProviderError::Transport),
                );
            }
            _ => unreachable!(),
        }
        let mut runtime = Runtime::new(pipeline);
        runtime.request(false).await;
        let output = runtime.terminal().await;
        assert_eq!(output.last().unwrap()["type"], "error");
        assert!(!output.iter().any(|v| v["event"] == "delta"));
        assert!(!json!(output).to_string().contains("PRIVATE"));
        let error = runtime.outcomes.recv().await.unwrap().unwrap_err();
        assert_eq!(
            error.stage,
            [
                RagStage::Coverage,
                RagStage::Retrieval,
                RagStage::FollowupRetrieval
            ][failure]
        );
        assert!(runtime.finish().await.is_empty());
    }
}
